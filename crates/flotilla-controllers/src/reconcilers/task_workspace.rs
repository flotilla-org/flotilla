use std::{collections::BTreeMap, marker::PhantomData};

use chrono::{DateTime, Utc};
use flotilla_resources::{
    canonicalize_repo_url, clone_key,
    controller::{Actuation, LabelJoinWatch, LabelMappedWatch, ReconcileOutcome, Reconciler, SecondaryWatch},
    descriptive_repo_slug, repo_key, Checkout, CheckoutPhase, CheckoutSpec, CheckoutWorktreeSpec, Clone, ClonePhase, CloneSpec, Convoy,
    DockerCheckoutStrategy, DockerEnvironmentSpec, Environment, EnvironmentMount, EnvironmentMountMode, EnvironmentPhase, EnvironmentSpec,
    FreshCloneCheckoutSpec, HostDirectPlacementPolicyCheckout, HostDirectPlacementPolicySpec, InputMeta, OwnerReference, PlacementPolicy,
    PlacementPolicySpec, ProcessDefinition, ProcessSource, Resource, ResourceBackend, ResourceError, ResourceObject, TaskWorkspace,
    TaskWorkspacePhase, TaskWorkspaceStatusPatch, TerminalSession, TerminalSessionPhase, TerminalSessionSpec, TypedResolver, CONVOY_LABEL,
    PROCESS_ORDINAL_LABEL, ROLE_LABEL, TASK_LABEL, TASK_ORDINAL_LABEL, TASK_WORKSPACE_LABEL,
};

const REPO_KEY_LABEL: &str = "flotilla.work/repo-key";
const REPO_LABEL: &str = "flotilla.work/repo";
const ENV_LABEL: &str = "flotilla.work/env";

pub struct TaskWorkspaceReconciler {
    convoys: TypedResolver<Convoy>,
    placement_policies: TypedResolver<PlacementPolicy>,
    environments: TypedResolver<Environment>,
    clones: TypedResolver<Clone>,
    checkouts: TypedResolver<Checkout>,
    terminal_sessions: TypedResolver<TerminalSession>,
}

impl TaskWorkspaceReconciler {
    pub fn new(backend: ResourceBackend, namespace: &str) -> Self {
        Self {
            convoys: backend.clone().using::<Convoy>(namespace),
            placement_policies: backend.clone().using::<PlacementPolicy>(namespace),
            environments: backend.clone().using::<Environment>(namespace),
            clones: backend.clone().using::<Clone>(namespace),
            checkouts: backend.clone().using::<Checkout>(namespace),
            terminal_sessions: backend.using::<TerminalSession>(namespace),
        }
    }

    pub fn secondary_watches() -> Vec<Box<dyn SecondaryWatch<Primary = TaskWorkspace>>> {
        vec![
            Box::new(LabelMappedWatch::<Environment, TaskWorkspace> { label_key: TASK_WORKSPACE_LABEL, _marker: PhantomData }),
            Box::new(LabelMappedWatch::<Checkout, TaskWorkspace> { label_key: TASK_WORKSPACE_LABEL, _marker: PhantomData }),
            Box::new(LabelMappedWatch::<TerminalSession, TaskWorkspace> { label_key: TASK_WORKSPACE_LABEL, _marker: PhantomData }),
            Box::new(LabelJoinWatch::<Clone, TaskWorkspace> { label_key: REPO_KEY_LABEL, _marker: PhantomData }),
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
    Provisioning { observed_policy_ref: String, observed_policy_version: String },
    Ready { environment_ref: String, checkout_ref: String, terminal_session_refs: Vec<String> },
    Failed { message: String },
}

#[derive(Debug, Clone)]
pub struct TaskWorkspaceDeps {
    patch: PlannedPatch,
    actuations: Vec<Actuation>,
}

impl TaskWorkspaceDeps {
    fn none() -> Self {
        Self { patch: PlannedPatch::None, actuations: Vec::new() }
    }

    fn provisioning(
        obj: &ResourceObject<TaskWorkspace>,
        placement_policy: &ResourceObject<PlacementPolicy>,
        actuations: Vec<Actuation>,
    ) -> Self {
        Self { patch: provisioning_patch(obj, placement_policy), actuations }
    }

    fn ready(environment_ref: String, checkout_ref: String, terminal_session_refs: Vec<String>, actuations: Vec<Actuation>) -> Self {
        Self { patch: PlannedPatch::Ready { environment_ref, checkout_ref, terminal_session_refs }, actuations }
    }

    fn failed(message: impl Into<String>) -> Self {
        Self { patch: PlannedPatch::Failed { message: message.into() }, actuations: Vec::new() }
    }
}

impl Reconciler for TaskWorkspaceReconciler {
    type Resource = TaskWorkspace;
    type Dependencies = TaskWorkspaceDeps;

    async fn fetch_dependencies(&self, obj: &ResourceObject<Self::Resource>) -> Result<Self::Dependencies, ResourceError> {
        if obj.status.as_ref().map(|status| status.phase) == Some(TaskWorkspacePhase::Failed) {
            return Ok(TaskWorkspaceDeps::none());
        }

        let convoy = match self.convoys.get(&obj.spec.convoy_ref).await {
            Ok(convoy) => convoy,
            Err(ResourceError::NotFound { .. }) => {
                return Ok(TaskWorkspaceDeps::failed(format!("convoy {} not found", obj.spec.convoy_ref)))
            }
            Err(err) => return Err(err),
        };
        let placement_policy = match self.placement_policies.get(&obj.spec.placement_policy_ref).await {
            Ok(policy) => policy,
            Err(ResourceError::NotFound { .. }) => {
                return Ok(TaskWorkspaceDeps::failed(format!("placement policy {} not found", obj.spec.placement_policy_ref)))
            }
            Err(err) => return Err(err),
        };
        let strategy = match placement_strategy(&placement_policy.spec) {
            Ok(strategy) => strategy,
            Err(message) => return Ok(TaskWorkspaceDeps::failed(message)),
        };

        let (task_index, task) = match convoy
            .status
            .as_ref()
            .and_then(|status| status.workflow_snapshot.as_ref())
            .and_then(|snapshot| snapshot.tasks.iter().enumerate().find(|(_, task)| task.name == obj.spec.task))
        {
            Some((task_index, task)) => (task_index, task),
            None => return Ok(TaskWorkspaceDeps::failed(format!("task {} missing from convoy snapshot", obj.spec.task))),
        };

        let repo_url = match convoy.spec.repository.as_ref().map(|repository| repository.url.clone()) {
            Some(url) => url,
            None => return Ok(TaskWorkspaceDeps::failed("convoy repository URL is missing".to_string())),
        };
        let git_ref = match convoy.spec.r#ref.clone() {
            Some(git_ref) => git_ref,
            None => return Ok(TaskWorkspaceDeps::failed("convoy ref is missing".to_string())),
        };
        let canonical_repo = match canonicalize_repo_url(&repo_url) {
            Ok(url) => url,
            Err(err) => return Ok(TaskWorkspaceDeps::failed(err)),
        };
        let repo_key = repo_key(&canonical_repo);
        let repo_slug = descriptive_repo_slug(&canonical_repo);

        let clone_env_ref = host_direct_environment_name(strategy.host_ref());
        let mut actuations = Vec::new();

        let clone_name = if strategy.needs_shared_clone() {
            let clone_env = match self.environments.get(&clone_env_ref).await {
                Ok(environment) => environment,
                Err(ResourceError::NotFound { .. }) => {
                    return Ok(TaskWorkspaceDeps::failed(format!("host-direct environment {clone_env_ref} not found")))
                }
                Err(err) => return Err(err),
            };
            let clone_env_spec = match clone_env.spec.host_direct.as_ref() {
                Some(spec) => spec,
                None => return Ok(TaskWorkspaceDeps::failed(format!("environment {clone_env_ref} is not a host_direct environment"))),
            };
            if clone_env.status.as_ref().map(|status| status.phase) != Some(EnvironmentPhase::Ready) {
                return Ok(TaskWorkspaceDeps::provisioning(obj, &placement_policy, actuations));
            }

            let clone_name = format!("clone-{}", clone_key(&canonical_repo, &clone_env_ref));
            let clone_path = format!("{}/{}", clone_env_spec.repo_default_dir.trim_end_matches('/'), repo_key);
            match self.clones.get(&clone_name).await {
                Ok(existing) => {
                    let existing_repo = match canonicalize_repo_url(&existing.spec.url) {
                        Ok(url) => url,
                        Err(err) => return Ok(TaskWorkspaceDeps::failed(err)),
                    };
                    if existing_repo != canonical_repo || existing.spec.env_ref != clone_env_ref {
                        return Ok(TaskWorkspaceDeps::failed(format!("clone {clone_name} does not match expected repo/env tuple")));
                    }
                    if existing.status.as_ref().map(|status| status.phase) == Some(ClonePhase::Failed) {
                        let message = existing
                            .status
                            .as_ref()
                            .and_then(|status| status.message.clone())
                            .unwrap_or_else(|| format!("clone {clone_name} failed"));
                        return Ok(TaskWorkspaceDeps::failed(message));
                    }
                    if existing.status.as_ref().map(|status| status.phase) != Some(ClonePhase::Ready) {
                        return Ok(TaskWorkspaceDeps::provisioning(obj, &placement_policy, actuations));
                    }
                }
                Err(ResourceError::NotFound { .. }) => {
                    actuations.push(Actuation::CreateClone {
                        meta: InputMeta::builder()
                            .name(clone_name.clone())
                            .labels(BTreeMap::from([
                                (REPO_KEY_LABEL.to_string(), repo_key.clone()),
                                (ENV_LABEL.to_string(), clone_env_ref.clone()),
                                (REPO_LABEL.to_string(), repo_slug.clone()),
                            ]))
                            .build(),
                        spec: CloneSpec { url: repo_url.clone(), env_ref: clone_env_ref.clone(), path: clone_path },
                    });
                    return Ok(TaskWorkspaceDeps::provisioning(obj, &placement_policy, actuations));
                }
                Err(err) => return Err(err),
            }

            Some(clone_name)
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
                            return Ok(TaskWorkspaceDeps::failed(message));
                        }
                        if existing.status.as_ref().map(|status| status.phase) != Some(EnvironmentPhase::Ready) {
                            return Ok(TaskWorkspaceDeps::provisioning(obj, &placement_policy, actuations));
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
                        return Ok(TaskWorkspaceDeps::provisioning(obj, &placement_policy, actuations));
                    }
                    Err(err) => return Err(err),
                }
                Some(env_name)
            }
            _ => None,
        };

        let checkout_name = checkout_name(&obj.metadata.name);
        let checkout_target_path = match &strategy {
            PlacementStrategy::HostDirect { .. } | PlacementStrategy::DockerWorktreeOnHostAndMount { .. } => {
                let clone_env = match self.environments.get(&clone_env_ref).await {
                    Ok(environment) => environment,
                    Err(ResourceError::NotFound { .. }) => {
                        return Ok(TaskWorkspaceDeps::failed(format!("host-direct environment {clone_env_ref} not found")))
                    }
                    Err(err) => return Err(err),
                };
                let clone_env_spec = match clone_env.spec.host_direct.as_ref() {
                    Some(spec) => spec,
                    None => return Ok(TaskWorkspaceDeps::failed(format!("environment {clone_env_ref} is not a host_direct environment"))),
                };
                checkout_target_path(&clone_env_spec.repo_default_dir, &repo_slug, &obj.metadata.name)
            }
            PlacementStrategy::DockerFreshCloneInContainer { clone_path, .. } => clone_path.clone(),
        };

        let checkout_ready_path;
        match self.checkouts.get(&checkout_name).await {
            Ok(existing) => {
                if existing.status.as_ref().map(|status| status.phase) == Some(CheckoutPhase::Failed) {
                    let message = existing
                        .status
                        .as_ref()
                        .and_then(|status| status.message.clone())
                        .unwrap_or_else(|| format!("checkout {checkout_name} failed"));
                    return Ok(TaskWorkspaceDeps::failed(message));
                }
                if existing.status.as_ref().map(|status| status.phase) == Some(CheckoutPhase::Ready) {
                    checkout_ready_path =
                        existing.status.as_ref().and_then(|status| status.path.clone()).unwrap_or(existing.spec.target_path);
                } else {
                    return Ok(TaskWorkspaceDeps::provisioning(obj, &placement_policy, actuations));
                }
            }
            Err(ResourceError::NotFound { .. }) => {
                let spec = match &strategy {
                    PlacementStrategy::HostDirect { .. } | PlacementStrategy::DockerWorktreeOnHostAndMount { .. } => CheckoutSpec {
                        env_ref: clone_env_ref.clone(),
                        r#ref: git_ref,
                        target_path: checkout_target_path.clone(),
                        worktree: Some(CheckoutWorktreeSpec {
                            clone_ref: clone_name.clone().expect("shared-clone strategy requires clone name"),
                        }),
                        fresh_clone: None,
                    },
                    PlacementStrategy::DockerFreshCloneInContainer { .. } => CheckoutSpec {
                        env_ref: precreated_environment_ref.clone().expect("fresh-clone strategy should precreate environment"),
                        r#ref: git_ref,
                        target_path: checkout_target_path.clone(),
                        worktree: None,
                        fresh_clone: Some(FreshCloneCheckoutSpec { url: repo_url.clone() }),
                    },
                };
                actuations.push(Actuation::CreateCheckout {
                    meta: owned_child_meta(&checkout_name, obj, BTreeMap::from([(ENV_LABEL.to_string(), spec.env_ref.clone())])),
                    spec,
                });
                return Ok(TaskWorkspaceDeps::provisioning(obj, &placement_policy, actuations));
            }
            Err(err) => return Err(err),
        }

        let resolved_environment_ref = match &strategy {
            PlacementStrategy::HostDirect { host_ref, .. } => {
                let env_name = host_direct_environment_name(host_ref);
                let environment = match self.environments.get(&env_name).await {
                    Ok(environment) => environment,
                    Err(ResourceError::NotFound { .. }) => {
                        return Ok(TaskWorkspaceDeps::failed(format!("host-direct environment {env_name} not found")))
                    }
                    Err(err) => return Err(err),
                };
                if environment.status.as_ref().map(|status| status.phase) == Some(EnvironmentPhase::Failed) {
                    return Ok(TaskWorkspaceDeps::failed(format!("environment {env_name} failed")));
                }
                if environment.status.as_ref().map(|status| status.phase) != Some(EnvironmentPhase::Ready) {
                    return Ok(TaskWorkspaceDeps::provisioning(obj, &placement_policy, actuations));
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
                            return Ok(TaskWorkspaceDeps::failed(message));
                        }
                        if existing.status.as_ref().map(|status| status.phase) != Some(EnvironmentPhase::Ready) {
                            return Ok(TaskWorkspaceDeps::provisioning(obj, &placement_policy, actuations));
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
                                    mounts: vec![EnvironmentMount {
                                        source_path: checkout_ready_path.clone(),
                                        target_path: mount_path.clone(),
                                        mode: EnvironmentMountMode::Rw,
                                    }],
                                    env: env.clone(),
                                }),
                            },
                        });
                        return Ok(TaskWorkspaceDeps::provisioning(obj, &placement_policy, actuations));
                    }
                    Err(err) => return Err(err),
                }
                env_name
            }
            PlacementStrategy::DockerFreshCloneInContainer { .. } => {
                precreated_environment_ref.expect("fresh-clone strategy should resolve environment first")
            }
        };

        let mut terminal_refs = Vec::new();
        for (process_index, process) in task.processes.iter().enumerate() {
            let command = match &process.source {
                ProcessSource::Tool { command } => command.clone(),
                ProcessSource::Agent { .. } => {
                    return Ok(TaskWorkspaceDeps::failed(format!(
                        "task {} contains agent process {}; Stage 4a supports tool processes only",
                        obj.spec.task, process.role
                    )))
                }
            };

            let terminal_name = terminal_name(&obj.metadata.name, &process.role);
            terminal_refs.push(terminal_name.clone());
            match self.terminal_sessions.get(&terminal_name).await {
                Ok(existing) => {
                    if existing.status.as_ref().map(|status| status.phase) == Some(TerminalSessionPhase::Stopped) {
                        let message = existing
                            .status
                            .as_ref()
                            .and_then(|status| status.message.clone())
                            .unwrap_or_else(|| format!("terminal session {terminal_name} stopped"));
                        return Ok(TaskWorkspaceDeps::failed(message));
                    }
                    if existing.status.as_ref().map(|status| status.phase) != Some(TerminalSessionPhase::Running) {
                        return Ok(TaskWorkspaceDeps::provisioning(obj, &placement_policy, actuations));
                    }
                }
                Err(ResourceError::NotFound { .. }) => {
                    actuations.push(Actuation::CreateTerminalSession {
                        meta: owned_child_meta(&terminal_name, obj, build_session_labels(obj, process, task_index, process_index)),
                        spec: TerminalSessionSpec {
                            env_ref: resolved_environment_ref.clone(),
                            role: process.role.clone(),
                            command,
                            cwd: strategy.terminal_cwd(&checkout_ready_path),
                            pool: strategy.pool().to_string(),
                        },
                    });
                    return Ok(TaskWorkspaceDeps::provisioning(obj, &placement_policy, actuations));
                }
                Err(err) => return Err(err),
            }
        }

        Ok(TaskWorkspaceDeps::ready(resolved_environment_ref, checkout_name, terminal_refs, actuations))
    }

    fn reconcile(
        &self,
        _obj: &ResourceObject<Self::Resource>,
        deps: &Self::Dependencies,
        now: DateTime<Utc>,
    ) -> ReconcileOutcome<Self::Resource> {
        let patch = match &deps.patch {
            PlannedPatch::None => None,
            PlannedPatch::Provisioning { observed_policy_ref, observed_policy_version } => {
                Some(TaskWorkspaceStatusPatch::MarkProvisioning {
                    observed_policy_ref: observed_policy_ref.clone(),
                    observed_policy_version: observed_policy_version.clone(),
                    started_at: now,
                })
            }
            PlannedPatch::Ready { environment_ref, checkout_ref, terminal_session_refs } => Some(TaskWorkspaceStatusPatch::MarkReady {
                environment_ref: Some(environment_ref.clone()),
                checkout_ref: Some(checkout_ref.clone()),
                terminal_session_refs: terminal_session_refs.clone(),
                ready_at: now,
            }),
            PlannedPatch::Failed { message } => Some(TaskWorkspaceStatusPatch::MarkFailed { message: message.clone() }),
        };

        ReconcileOutcome::with_actuations(patch, deps.actuations.clone())
    }

    async fn run_finalizer(&self, _obj: &ResourceObject<Self::Resource>) -> Result<(), ResourceError> {
        let selector = BTreeMap::from([(TASK_WORKSPACE_LABEL.to_string(), _obj.metadata.name.clone())]);

        delete_matching(&self.terminal_sessions, &selector).await?;
        delete_matching(&self.checkouts, &selector).await?;
        delete_matching(&self.environments, &selector).await?;

        Ok(())
    }

    fn finalizer_name(&self) -> Option<&'static str> {
        Some("flotilla.work/task-workspace-teardown")
    }
}

fn placement_strategy(spec: &PlacementPolicySpec) -> Result<PlacementStrategy, String> {
    if let Some(HostDirectPlacementPolicySpec { host_ref, checkout: HostDirectPlacementPolicyCheckout::Worktree }) = &spec.host_direct {
        return Ok(PlacementStrategy::HostDirect { host_ref: host_ref.clone(), pool: spec.pool.clone() });
    }

    if let Some(docker) = &spec.docker_per_task {
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

fn provisioning_patch(obj: &ResourceObject<TaskWorkspace>, placement_policy: &ResourceObject<PlacementPolicy>) -> PlannedPatch {
    if obj.status.as_ref().map(|status| status.phase) == Some(TaskWorkspacePhase::Provisioning) {
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

fn environment_name(task_workspace_name: &str) -> String {
    format!("env-{task_workspace_name}")
}

fn checkout_name(task_workspace_name: &str) -> String {
    format!("checkout-{task_workspace_name}")
}

fn terminal_name(task_workspace_name: &str, role: &str) -> String {
    format!("terminal-{task_workspace_name}-{role}")
}

fn checkout_target_path(repo_default_dir: &str, repo_slug: &str, task_workspace_name: &str) -> String {
    format!("{}/{}.{}", repo_default_dir.trim_end_matches('/'), repo_slug, task_workspace_name)
}

fn owned_child_meta(name: &str, workspace: &ResourceObject<TaskWorkspace>, mut extra_labels: BTreeMap<String, String>) -> InputMeta {
    extra_labels.insert(TASK_WORKSPACE_LABEL.to_string(), workspace.metadata.name.clone());
    InputMeta::builder()
        .name(name.to_string())
        .labels(extra_labels)
        .owner_references(vec![OwnerReference {
            api_version: format!("{}/{}", TaskWorkspace::API_PATHS.group, TaskWorkspace::API_PATHS.version),
            kind: TaskWorkspace::API_PATHS.kind.to_string(),
            name: workspace.metadata.name.clone(),
            controller: true,
        }])
        .build()
}

fn build_session_labels(
    workspace: &ResourceObject<TaskWorkspace>,
    process: &ProcessDefinition,
    task_index: usize,
    process_index: usize,
) -> BTreeMap<String, String> {
    let mut labels = process.labels.clone();
    labels.insert(TASK_WORKSPACE_LABEL.to_string(), workspace.metadata.name.clone());
    labels.insert(ROLE_LABEL.to_string(), process.role.clone());
    labels.insert(CONVOY_LABEL.to_string(), workspace.spec.convoy_ref.clone());
    labels.insert(TASK_LABEL.to_string(), workspace.spec.task.clone());
    labels.insert(TASK_ORDINAL_LABEL.to_string(), format!("{task_index:03}"));
    labels.insert(PROCESS_ORDINAL_LABEL.to_string(), format!("{process_index:03}"));
    labels
}

async fn delete_matching<T: Resource>(resolver: &TypedResolver<T>, selector: &BTreeMap<String, String>) -> Result<(), ResourceError> {
    let listed = resolver.list_matching_labels(selector).await?;
    for object in listed.items {
        match resolver.delete(&object.metadata.name).await {
            Ok(()) | Err(ResourceError::NotFound { .. }) => {}
            Err(err) => return Err(err),
        }
    }
    Ok(())
}

impl PlacementStrategy {
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

    fn terminal_cwd(&self, checkout_path: &str) -> String {
        match self {
            Self::HostDirect { .. } => checkout_path.to_string(),
            Self::DockerWorktreeOnHostAndMount { mount_path, default_cwd, .. } => default_cwd.clone().unwrap_or_else(|| mount_path.clone()),
            Self::DockerFreshCloneInContainer { clone_path, default_cwd, .. } => default_cwd.clone().unwrap_or_else(|| clone_path.clone()),
        }
    }
}
