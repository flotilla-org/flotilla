use std::{collections::BTreeMap, marker::PhantomData};

use chrono::{DateTime, Utc};
use flotilla_core::agent_adapter::{build_crew_brief, CrewBriefMember};
use flotilla_resources::{
    clone_key,
    controller::{
        delete_lifecycle_owned_matching, Actuation, LabelJoinWatch, LabelMappedWatch, ReconcileOutcome, Reconciler, SecondaryWatch,
    },
    Checkout, CheckoutPhase, CheckoutSpec, CheckoutWorktreeSpec, Clone, ClonePhase, CloneSpec, Convoy, CrewSource, DockerCheckoutStrategy,
    DockerEnvironmentSpec, Environment, EnvironmentMount, EnvironmentMountMode, EnvironmentPhase, EnvironmentSpec, FreshCloneCheckoutSpec,
    HostDirectPlacementPolicyCheckout, HostDirectPlacementPolicySpec, InputMeta, LifecycleAuthority, OwnerReference, PlacementPolicy,
    PlacementPolicySpec, Repository, RepositoryIdentity, RepositoryKey, Resource, ResourceBackend, ResourceError, ResourceObject, Stance,
    TerminalSession, TerminalSessionIdentity, TerminalSessionPhase, TerminalSessionSpec, TypedResolver, Vessel, VesselPhase,
    VesselStatusPatch, CONVOY_LABEL, VESSEL_REF_LABEL,
};

const REPO_KEY_LABEL: &str = "flotilla.work/repo-key";
const REPO_LABEL: &str = "flotilla.work/repo";
const ENV_LABEL: &str = "flotilla.work/env";

#[derive(bon::Builder)]
pub struct VesselReconciler {
    convoys: TypedResolver<Convoy>,
    repositories: TypedResolver<Repository>,
    placement_policies: TypedResolver<PlacementPolicy>,
    environments: TypedResolver<Environment>,
    clones: TypedResolver<Clone>,
    checkouts: TypedResolver<Checkout>,
    terminal_sessions: TypedResolver<TerminalSession>,
    namespace: String,
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
            namespace: namespace.to_string(),
        }
    }

    pub fn secondary_watches() -> Vec<Box<dyn SecondaryWatch<Primary = Vessel>>> {
        vec![
            Box::new(LabelMappedWatch::<Environment, Vessel> { label_key: VESSEL_REF_LABEL, _marker: PhantomData }),
            Box::new(LabelMappedWatch::<Checkout, Vessel> { label_key: VESSEL_REF_LABEL, _marker: PhantomData }),
            Box::new(LabelJoinWatch::<Checkout, Vessel> { label_key: CONVOY_LABEL, _marker: PhantomData }),
            Box::new(LabelMappedWatch::<TerminalSession, Vessel> { label_key: VESSEL_REF_LABEL, _marker: PhantomData }),
        ]
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
        env: BTreeMap<String, String>,
        mount_path: String,
        default_cwd: Option<String>,
    },
    DockerFreshCloneInContainer {
        host_ref: String,
        pool: String,
        image: String,
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
    },
    Ready {
        environment_ref: String,
        checkout_refs: BTreeMap<RepositoryKey, String>,
        terminal_session_refs: Vec<String>,
        requested_stance: Stance,
        effective_stance: Stance,
    },
    Failed {
        message: String,
    },
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

    fn provisioning(obj: &ResourceObject<Vessel>, placement_policy: &ResourceObject<PlacementPolicy>, actuations: Vec<Actuation>) -> Self {
        Self { patch: provisioning_patch(obj, placement_policy), actuations }
    }

    fn ready(
        environment_ref: String,
        checkout_refs: BTreeMap<RepositoryKey, String>,
        terminal_session_refs: Vec<String>,
        requested_stance: Stance,
        effective_stance: Stance,
        actuations: Vec<Actuation>,
    ) -> Self {
        Self {
            patch: PlannedPatch::Ready { environment_ref, checkout_refs, terminal_session_refs, requested_stance, effective_stance },
            actuations,
        }
    }

    fn failed(message: impl Into<String>) -> Self {
        Self { patch: PlannedPatch::Failed { message: message.into() }, actuations: Vec::new() }
    }
}

impl Reconciler for VesselReconciler {
    type Resource = Vessel;
    type Dependencies = VesselDeps;

    async fn fetch_dependencies(&self, obj: &ResourceObject<Self::Resource>) -> Result<Self::Dependencies, ResourceError> {
        if obj.status.as_ref().map(|status| status.phase) == Some(VesselPhase::Failed) {
            return Ok(VesselDeps::none());
        }

        let convoy = match self.convoys.get(&obj.spec.convoy_ref).await {
            Ok(convoy) => convoy,
            Err(ResourceError::NotFound { .. }) => return Ok(VesselDeps::failed(format!("convoy {} not found", obj.spec.convoy_ref))),
            Err(err) => return Err(err),
        };
        let placement_policy = match self.placement_policies.get(&obj.spec.placement_policy_ref).await {
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

        let (vessel_index, requirement) = match convoy
            .status
            .as_ref()
            .and_then(|status| status.workflow_snapshot.as_ref())
            .and_then(|snapshot| snapshot.vessels.iter().enumerate().find(|(_, vessel)| vessel.name == obj.spec.vessel_name))
        {
            Some((vessel_index, requirement)) => (vessel_index, requirement),
            None => return Ok(VesselDeps::failed(format!("vessel {} missing from convoy snapshot", obj.spec.vessel_name))),
        };
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
                    return Ok(VesselDeps::failed(format!("host-direct environment {clone_env_ref} not found")))
                }
                Err(err) => return Err(err),
            };
            let clone_env_spec = match clone_env.spec.host_direct.as_ref() {
                Some(spec) => spec,
                None => return Ok(VesselDeps::failed(format!("environment {clone_env_ref} is not a host_direct environment"))),
            };
            if clone_env.status.as_ref().map(|status| status.phase) != Some(EnvironmentPhase::Ready) {
                return Ok(VesselDeps::provisioning(obj, &placement_policy, actuations));
            }
            Some(clone_env_spec.repo_default_dir.clone())
        } else {
            None
        };

        let precreated_environment_ref = match &strategy {
            PlacementStrategy::DockerFreshCloneInContainer { host_ref, image, env, .. } => {
                let env_name = environment_name(&obj.metadata.name);
                match self.environments.get(&env_name).await {
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
                            return Ok(VesselDeps::provisioning(obj, &placement_policy, actuations));
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
                                    mounts: Vec::new(),
                                    env: env.clone(),
                                }),
                            },
                        });
                        return Ok(VesselDeps::provisioning(obj, &placement_policy, actuations));
                    }
                    Err(err) => return Err(err),
                }
                Some(env_name)
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
        let mut waiting_for_checkouts = false;
        for convoy_repository in convoy_repositories {
            let repository = match self.repositories.get(&convoy_repository.repo_ref.to_string()).await {
                Ok(repository) => repository,
                Err(ResourceError::NotFound { .. }) => {
                    return Ok(VesselDeps::failed(format!("repository {} not found", convoy_repository.repo_ref)))
                }
                Err(err) => return Err(err),
            };
            if let Err(message) = repository.spec.verify_key(&convoy_repository.repo_ref) {
                return Ok(VesselDeps::failed(message));
            }
            let canonical_repo = match repository.spec.identity() {
                RepositoryIdentity::Remote { canonical_remote } => canonical_remote.clone(),
                RepositoryIdentity::Local { .. } => return Ok(VesselDeps::failed("convoy repository must have a transport remote")),
            };
            let repository_key = convoy_repository.repo_ref.clone();
            let repo_key = repository_key.to_string();
            let adopted_checkout_ref = obj.spec.adopted_checkout_refs.get(&repository_key).cloned();
            let clone_name = if adopted_checkout_ref.is_none() && strategy.needs_shared_clone() {
                let clone_name = format!("clone-{}", clone_key(&canonical_repo, &clone_env_ref));
                let clone_path = format!(
                    "{}/{}",
                    shared_clone_root.as_deref().expect("shared-clone placement has a root").trim_end_matches('/'),
                    repo_key
                );
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
                    Err(ResourceError::NotFound { .. }) => actuations.push(Actuation::CreateClone {
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
                    }),
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
                            &repository.spec.catalog_slug(),
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
                        checkout_refs.insert(repository_key.clone(), checkout_name);
                        checkout_paths.insert(repository_key, path);
                    } else {
                        waiting_for_checkouts = true;
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
                                base_ref: Some(convoy_repository.base_ref.clone()),
                                target_path: checkout_target_path,
                                clone_ref: clone_name.expect("shared-clone strategy requires clone name"),
                            })
                        }
                        PlacementStrategy::DockerFreshCloneInContainer { .. } => CheckoutSpec::FreshClone(FreshCloneCheckoutSpec {
                            repo_ref: repository_key,
                            env_ref: precreated_environment_ref.clone().expect("fresh-clone strategy should precreate environment"),
                            r#ref: git_ref.clone().expect("repository checkout requires a convoy ref"),
                            base_ref: Some(convoy_repository.base_ref.clone()),
                            target_path: checkout_target_path,
                            url: convoy_repository.url.clone(),
                        }),
                    };
                    let env_ref = spec.env_ref().expect("managed checkout should have an env_ref").to_string();
                    let labels = BTreeMap::from([(ENV_LABEL.to_string(), env_ref), (REPO_KEY_LABEL.to_string(), repo_key)]);
                    let meta = match &strategy {
                        PlacementStrategy::HostDirect { .. } | PlacementStrategy::DockerWorktreeOnHostAndMount { .. } => {
                            convoy_owned_child_meta(&checkout_name, &convoy, labels)
                        }
                        PlacementStrategy::DockerFreshCloneInContainer { .. } => owned_child_meta(&checkout_name, obj, labels),
                    };
                    actuations.push(Actuation::CreateCheckout { meta, spec });
                    waiting_for_checkouts = true;
                }
                Err(err) => return Err(err),
            }
        }
        if waiting_for_checkouts {
            return Ok(VesselDeps::provisioning(obj, &placement_policy, actuations));
        }
        let has_repositories = !repository_refs.is_empty();
        let workspace_root = if multi_repository {
            workspace_root
        } else if let Some(path) = checkout_paths.values().next() {
            path.clone()
        } else {
            shared_clone_root.clone().unwrap_or_default()
        };

        let resolved_environment_ref = match &strategy {
            PlacementStrategy::HostDirect { host_ref, .. } => {
                let env_name = host_direct_environment_name(host_ref);
                let environment = match self.environments.get(&env_name).await {
                    Ok(environment) => environment,
                    Err(ResourceError::NotFound { .. }) => {
                        return Ok(VesselDeps::failed(format!("host-direct environment {env_name} not found")))
                    }
                    Err(err) => return Err(err),
                };
                if environment.status.as_ref().map(|status| status.phase) == Some(EnvironmentPhase::Failed) {
                    return Ok(VesselDeps::failed(format!("environment {env_name} failed")));
                }
                if environment.status.as_ref().map(|status| status.phase) != Some(EnvironmentPhase::Ready) {
                    return Ok(VesselDeps::provisioning(obj, &placement_policy, actuations));
                }
                env_name
            }
            PlacementStrategy::DockerWorktreeOnHostAndMount { host_ref, image, env, mount_path, .. } => {
                let env_name = environment_name(&obj.metadata.name);
                match self.environments.get(&env_name).await {
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
                            return Ok(VesselDeps::provisioning(obj, &placement_policy, actuations));
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
                                    mounts: has_repositories
                                        .then(|| EnvironmentMount {
                                            source_path: workspace_root.clone(),
                                            target_path: mount_path.clone(),
                                            mode: EnvironmentMountMode::Rw,
                                        })
                                        .into_iter()
                                        .collect(),
                                    env: env.clone(),
                                }),
                            },
                        });
                        return Ok(VesselDeps::provisioning(obj, &placement_policy, actuations));
                    }
                    Err(err) => return Err(err),
                }
                env_name
            }
            PlacementStrategy::DockerFreshCloneInContainer { .. } => {
                precreated_environment_ref.expect("fresh-clone strategy should resolve environment first")
            }
        };
        let terminal_cwd = strategy.terminal_cwd(&workspace_root, multi_repository, has_repositories);
        let brief_copies = checkout_paths
            .iter()
            .filter(|(repo_ref, _)| {
                convoy.spec.issue.as_ref().and_then(|issue| issue.repository_ref.as_ref()).is_none_or(|relevant| relevant == *repo_ref)
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

        let first_agent_index = requirement.crew.iter().position(|process| matches!(process.source, CrewSource::Agent { .. }));
        let mut terminal_refs = Vec::new();
        for (crew_index, process) in requirement.crew.iter().enumerate() {
            let should_start = matches!(process.source, CrewSource::Tool { .. }) || first_agent_index == Some(crew_index);
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
                    if phase == Some(TerminalSessionPhase::Failed)
                        || (phase == Some(TerminalSessionPhase::Stopped) && matches!(process.source, CrewSource::Tool { .. }))
                    {
                        let message = existing
                            .status
                            .as_ref()
                            .and_then(|status| status.message.clone())
                            .unwrap_or_else(|| format!("terminal session {terminal_name} stopped"));
                        return Ok(VesselDeps::failed(message));
                    }
                    if phase == Some(TerminalSessionPhase::Stopped) {
                        continue;
                    }
                    if should_start && phase != Some(TerminalSessionPhase::Running) {
                        return Ok(VesselDeps::provisioning(obj, &placement_policy, actuations));
                    }
                }
                Err(ResourceError::NotFound { .. }) => {
                    if !should_start {
                        continue;
                    }
                    let source = match &process.source {
                        CrewSource::Tool { command } => flotilla_resources::TerminalSessionSource::Tool { command: command.clone() },
                        CrewSource::Agent { selector, prompt } => {
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
                                    state: if matches!(member.source, CrewSource::Tool { .. }) || first_agent_index == Some(index) {
                                        "active"
                                    } else {
                                        "latent"
                                    }
                                    .to_string(),
                                    is_agent: matches!(member.source, CrewSource::Agent { .. }),
                                })
                                .collect::<Vec<_>>();
                            let mut brief = build_crew_brief(&context, &obj.spec.vessel_name, &process.role, prompt.as_deref(), &members);
                            append_convoy_work_context(&mut brief.content, &convoy, &repository_refs);
                            brief.copies = brief_copies.clone();
                            flotilla_resources::TerminalSessionSource::Agent { selector: selector.clone(), brief, context, message: None }
                        }
                    };
                    actuations.push(Actuation::CreateTerminalSession {
                        meta: identity.input_meta(),
                        spec: TerminalSessionSpec {
                            env_ref: resolved_environment_ref.clone(),
                            role: process.role.clone(),
                            source,
                            cwd: terminal_cwd.clone(),
                            pool: strategy.pool().to_string(),
                        },
                    });
                    return Ok(VesselDeps::provisioning(obj, &placement_policy, actuations));
                }
                Err(err) => return Err(err),
            }
        }

        Ok(VesselDeps::ready(resolved_environment_ref, checkout_refs, terminal_refs, requirement.stance, effective_stance, actuations))
    }

    fn reconcile(
        &self,
        _obj: &ResourceObject<Self::Resource>,
        deps: &Self::Dependencies,
        now: DateTime<Utc>,
    ) -> ReconcileOutcome<Self::Resource> {
        let patch = match &deps.patch {
            PlannedPatch::None => None,
            PlannedPatch::Provisioning { observed_policy_ref, observed_policy_version } => Some(VesselStatusPatch::MarkProvisioning {
                observed_policy_ref: observed_policy_ref.clone(),
                observed_policy_version: observed_policy_version.clone(),
                started_at: now,
            }),
            PlannedPatch::Ready { environment_ref, checkout_refs, terminal_session_refs, requested_stance, effective_stance } => {
                Some(VesselStatusPatch::MarkReady {
                    environment_ref: Some(environment_ref.clone()),
                    checkout_refs: checkout_refs.clone(),
                    terminal_session_refs: terminal_session_refs.clone(),
                    requested_stance: *requested_stance,
                    effective_stance: *effective_stance,
                    ready_at: now,
                })
            }
            PlannedPatch::Failed { message } => Some(VesselStatusPatch::MarkFailed { message: message.clone() }),
        };

        ReconcileOutcome::with_actuations(patch, deps.actuations.clone())
    }

    async fn run_finalizer(&self, obj: &ResourceObject<Self::Resource>) -> Result<(), ResourceError> {
        let selector = BTreeMap::from([(VESSEL_REF_LABEL.to_string(), obj.metadata.name.clone())]);

        delete_lifecycle_owned_matching(&self.terminal_sessions, &selector).await?;
        delete_lifecycle_owned_matching(&self.checkouts, &selector).await?;
        delete_lifecycle_owned_matching(&self.environments, &selector).await?;

        Ok(())
    }

    fn finalizer_name(&self) -> Option<&'static str> {
        Some("flotilla.work/vessel-workspace-teardown")
    }
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
                env: docker.env.clone(),
                mount_path: mount_path.clone(),
                default_cwd: docker.default_cwd.clone(),
            }),
            DockerCheckoutStrategy::FreshCloneInContainer { clone_path } => Ok(PlacementStrategy::DockerFreshCloneInContainer {
                host_ref: docker.host_ref.clone(),
                pool: spec.pool.clone(),
                image: docker.image.clone(),
                env: docker.env.clone(),
                clone_path: clone_path.clone(),
                default_cwd: docker.default_cwd.clone(),
            }),
        };
    }

    Err("placement policy must define exactly one supported strategy".to_string())
}

fn provisioning_patch(obj: &ResourceObject<Vessel>, placement_policy: &ResourceObject<PlacementPolicy>) -> PlannedPatch {
    if obj.status.as_ref().map(|status| status.phase) == Some(VesselPhase::Provisioning) {
        PlannedPatch::None
    } else {
        PlannedPatch::Provisioning {
            observed_policy_ref: placement_policy.metadata.name.clone(),
            observed_policy_version: placement_policy.metadata.resource_version.clone(),
        }
    }
}

fn host_direct_environment_name(host_ref: &str) -> String {
    format!("host-direct-{host_ref}")
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

fn append_convoy_work_context(content: &mut String, convoy: &ResourceObject<Convoy>, repository_refs: &[RepositoryKey]) {
    content.push_str("\n\n## Work context\n\n");
    if let Some(branch) = &convoy.spec.r#ref {
        content.push_str(&format!("- Branch: `{branch}`\n"));
    }
    content.push_str("- Repositories:\n");
    for repository in convoy.spec.repositories.iter().filter(|repository| repository_refs.contains(&repository.repo_ref)) {
        content.push_str(&format!("  - `{}` — {}\n", repository.repo_ref, repository.url));
    }
    if let Some(issue) = &convoy.spec.issue {
        content.push_str("\n## Issue snapshot\n\n");
        content.push_str(&format!(
            "Source-qualified reference: `{}` / `{}` / `{}`\n\n",
            issue.reference.source.service, issue.reference.source.scope, issue.reference.id
        ));
        content.push_str(&format!("Snapshot as of `{}`.\n\n", issue.snapshot.as_of.to_rfc3339()));
        content.push_str(&format!("### {}\n\n", issue.snapshot.title));
        content.push_str(&format!("State: `{:?}`\n\n", issue.snapshot.state).to_lowercase());
        content.push_str(&format!("Labels: {}\n\n", issue.snapshot.labels.join(", ")));
        if let Some(body) = &issue.snapshot.body {
            content.push_str(body);
            content.push('\n');
        }
    }
    if let Some(instruction) = &convoy.spec.instruction {
        content.push_str("\n## Human instruction\n\n");
        content.push_str(instruction);
        content.push('\n');
    }
}

fn owned_child_meta(name: &str, workspace: &ResourceObject<Vessel>, mut extra_labels: BTreeMap<String, String>) -> InputMeta {
    extra_labels.insert(VESSEL_REF_LABEL.to_string(), workspace.metadata.name.clone());
    InputMeta::builder()
        .name(name.to_string())
        .labels(extra_labels)
        .owner_references(vec![OwnerReference {
            api_version: format!("{}/{}", Vessel::API_PATHS.group, Vessel::API_PATHS.version),
            kind: Vessel::API_PATHS.kind.to_string(),
            name: workspace.metadata.name.clone(),
            controller: true,
        }])
        .build()
}

fn convoy_owned_child_meta(name: &str, convoy: &ResourceObject<Convoy>, mut extra_labels: BTreeMap<String, String>) -> InputMeta {
    extra_labels.insert(CONVOY_LABEL.to_string(), convoy.metadata.name.clone());
    InputMeta::builder()
        .name(name.to_string())
        .labels(extra_labels)
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
