use std::{
    collections::{BTreeMap, HashMap},
    fmt::Write,
    marker::PhantomData,
    sync::Arc,
};

use async_trait::async_trait;
use flotilla_core::{
    hop_chain::{
        builder::HopPlanBuilder,
        environment::{DockerEnvironmentHopResolver, NoopEnvironmentHopResolver},
        remote::ssh_resolver_from_config,
        resolver::{AlwaysWrap, HopResolver},
        terminal::NoopTerminalHopResolver,
        Hop, ResolutionContext, ResolvedAction,
    },
    path_context::{DaemonHostPath, ExecutionEnvironmentPath},
    providers::{registry::ProviderRegistry, types::WorkspaceAttachRequest},
    HostName,
};
use flotilla_protocol::{arg, EnvironmentId};
use flotilla_resources::{
    controller::{LabelJoinWatch, ReconcileOutcome, Reconciler, SecondaryWatch},
    Environment, Host, Presentation, PresentationStatus, PresentationStatusPatch, ResourceBackend, ResourceError, ResourceObject,
    TerminalSession, TypedResolver, CONVOY_LABEL,
};
use sha2::{Digest, Sha256};
use tracing::warn;

type RegistryLookup = dyn Fn(&str) -> Result<Arc<ProviderRegistry>, String> + Send + Sync;

#[async_trait]
pub trait PresentationRuntime: Send + Sync {
    async fn apply(&self, plan: &PresentationPlan) -> Result<AppliedPresentation, ApplyPresentationError>;
    async fn tear_down(&self, manager: &str, workspace_ref: &str) -> Result<(), String>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PresentationPlan {
    pub policy: String,
    pub name: String,
    pub processes: Vec<ResolvedProcess>,
    pub presentation_local_cwd: ExecutionEnvironmentPath,
    pub previous: Option<PreviousWorkspace>,
    pub spec_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviousWorkspace {
    pub presentation_manager: String,
    pub workspace_ref: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppliedPresentation {
    pub presentation_manager: String,
    pub workspace_ref: String,
    pub spec_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApplyPresentationError {
    UnknownPolicy(String),
    RetryFromCleanSlate(String),
    Failed(String),
}

pub trait PresentationPolicy: Send + Sync {
    fn name(&self) -> &'static str;
    fn render(&self, processes: &[ResolvedProcess], context: &PolicyContext) -> RenderedWorkspace;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyContext {
    pub name: String,
    pub presentation_local_cwd: ExecutionEnvironmentPath,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedProcess {
    pub role: String,
    pub labels: BTreeMap<String, String>,
    pub attach_command: String,
}

#[derive(Debug, Clone)]
pub struct RenderedWorkspace {
    pub attach_request: WorkspaceAttachRequest,
}

pub struct PresentationPolicyRegistry {
    policies: HashMap<String, Arc<dyn PresentationPolicy>>,
}

impl PresentationPolicyRegistry {
    pub fn with_defaults() -> Self {
        let mut policies = HashMap::new();
        let default = Arc::new(DefaultPolicy) as Arc<dyn PresentationPolicy>;
        policies.insert(default.name().to_string(), default);
        Self { policies }
    }

    pub fn resolve(&self, name: &str) -> Option<&Arc<dyn PresentationPolicy>> {
        self.policies.get(name)
    }
}

impl Default for PresentationPolicyRegistry {
    fn default() -> Self {
        Self::with_defaults()
    }
}

pub struct DefaultPolicy;

impl PresentationPolicy for DefaultPolicy {
    fn name(&self) -> &'static str {
        "default"
    }

    fn render(&self, processes: &[ResolvedProcess], context: &PolicyContext) -> RenderedWorkspace {
        let mut roles = Vec::new();
        for process in processes {
            if !roles.iter().any(|role| role == &process.role) {
                roles.push(process.role.clone());
            }
        }

        RenderedWorkspace {
            attach_request: WorkspaceAttachRequest {
                name: context.name.clone(),
                working_directory: context.presentation_local_cwd.clone(),
                template_vars: HashMap::new(),
                template_yaml: Some(default_policy_template_yaml(&roles)),
                attach_commands: processes.iter().map(|process| (process.role.clone(), process.attach_command.clone())).collect(),
            },
        }
    }
}

#[derive(Clone)]
pub struct HopChainContext {
    local_host_ref: String,
    local_host: HostName,
    config_base: DaemonHostPath,
    repo_root: Option<ExecutionEnvironmentPath>,
    registry_lookup: Arc<RegistryLookup>,
}

impl HopChainContext {
    pub fn new<F>(local_host_ref: impl Into<String>, local_host: HostName, config_base: DaemonHostPath, registry_lookup: F) -> Self
    where
        F: Fn(&str) -> Result<Arc<ProviderRegistry>, String> + Send + Sync + 'static,
    {
        Self { local_host_ref: local_host_ref.into(), local_host, config_base, repo_root: None, registry_lookup: Arc::new(registry_lookup) }
    }

    pub fn with_repo_root(mut self, repo_root: ExecutionEnvironmentPath) -> Self {
        self.repo_root = Some(repo_root);
        self
    }

    fn registry_for_env(&self, env_ref: &str) -> Result<Arc<ProviderRegistry>, String> {
        (self.registry_lookup)(env_ref)
    }

    fn local_host(&self) -> &HostName {
        &self.local_host
    }

    fn config_base(&self) -> &DaemonHostPath {
        &self.config_base
    }

    fn presentation_local_cwd(&self) -> ExecutionEnvironmentPath {
        if let Some(repo_root) = self.repo_root.as_ref().filter(|path| path.as_path().exists()) {
            return repo_root.clone();
        }
        if let Some(home) = dirs::home_dir() {
            return ExecutionEnvironmentPath::new(home);
        }
        // This only fires in degenerate environments where neither an injected repo root nor a
        // home directory is available, so the process cwd is our last meaningful local fallback.
        if let Ok(cwd) = std::env::current_dir() {
            return ExecutionEnvironmentPath::new(cwd);
        }
        ExecutionEnvironmentPath::new(self.config_base.as_path())
    }

    fn target_host(&self, host_ref: &str) -> HostName {
        if host_ref == self.local_host_ref {
            self.local_host.clone()
        } else {
            HostName::new(host_ref)
        }
    }
}

pub struct PresentationReconciler<R> {
    runtime: Arc<R>,
    terminal_sessions: TypedResolver<TerminalSession>,
    environments: TypedResolver<Environment>,
    hosts: TypedResolver<Host>,
    hop_chain: HopChainContext,
    policies: Arc<PresentationPolicyRegistry>,
}

impl<R> PresentationReconciler<R> {
    pub fn new(
        runtime: Arc<R>,
        backend: ResourceBackend,
        namespace: &str,
        hop_chain: HopChainContext,
        policies: Arc<PresentationPolicyRegistry>,
    ) -> Self {
        Self {
            runtime,
            terminal_sessions: backend.clone().using::<TerminalSession>(namespace),
            environments: backend.clone().using::<Environment>(namespace),
            hosts: backend.using::<Host>(namespace),
            hop_chain,
            policies,
        }
    }

    pub fn secondary_watches() -> Vec<Box<dyn SecondaryWatch<Primary = Presentation>>> {
        vec![Box::new(LabelJoinWatch::<TerminalSession, Presentation> { label_key: CONVOY_LABEL, _marker: PhantomData })]
    }

    async fn resolve_process(&self, session: &ResourceObject<TerminalSession>) -> Result<ResolvedProcess, String> {
        let environment = self
            .environments
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
        self.hosts.get(host_ref).await.map_err(|err| format!("host {} lookup failed: {err}", host_ref))?;

        let registry = self.hop_chain.registry_for_env(&session.spec.env_ref)?;
        let pool = registry
            .terminal_pools
            .get(&session.spec.pool)
            .map(|(_, pool)| Arc::clone(pool))
            .or_else(|| registry.terminal_pools.preferred().cloned())
            .ok_or_else(|| format!("terminal pool {} unavailable for environment {}", session.spec.pool, session.spec.env_ref))?;

        let session_name =
            session.status.as_ref().and_then(|status| status.session_id.as_deref()).unwrap_or(session.metadata.name.as_str());
        let session_cwd = ExecutionEnvironmentPath::new(&session.spec.cwd);
        let attach_args = pool.attach_args(session_name, &session.spec.command, &session_cwd, &Vec::new())?;
        let attach_command = self.build_attach_command(session, &environment, host_ref, attach_args)?;

        Ok(ResolvedProcess { role: session.spec.role.clone(), labels: session.metadata.labels.clone(), attach_command })
    }

    fn build_attach_command(
        &self,
        session: &ResourceObject<TerminalSession>,
        environment: &ResourceObject<Environment>,
        host_ref: &str,
        attach_args: Vec<arg::Arg>,
    ) -> Result<String, String> {
        let ssh_resolver = ssh_resolver_from_config(self.hop_chain.config_base())?;
        let environment_id = EnvironmentId::new(session.spec.env_ref.clone());
        let env_resolver: Arc<dyn flotilla_core::hop_chain::environment::EnvironmentHopResolver> =
            if let Some(container_id) = environment.status.as_ref().and_then(|status| status.docker_container_id.as_ref()) {
                Arc::new(DockerEnvironmentHopResolver::new(HashMap::from([(environment_id.clone(), container_id.clone())])))
            } else {
                Arc::new(NoopEnvironmentHopResolver)
            };
        let hop_resolver = HopResolver {
            remote: Arc::new(ssh_resolver),
            environment: env_resolver,
            terminal: Arc::new(NoopTerminalHopResolver),
            strategy: Arc::new(AlwaysWrap),
        };
        let target_host = self.hop_chain.target_host(host_ref);
        let mut plan = HopPlanBuilder::new(self.hop_chain.local_host()).build_for_prepared_command(&target_host, &attach_args);
        if environment.spec.docker.is_some() {
            let run_command_index = plan.0.iter().position(|hop| matches!(hop, Hop::RunCommand { .. })).unwrap_or(plan.0.len());
            plan.0.insert(run_command_index, Hop::EnterEnvironment { env_id: environment_id, provider: "docker".to_string() });
        }
        let mut context = ResolutionContext {
            current_host: self.hop_chain.local_host().clone(),
            current_environment: None,
            working_directory: Some(ExecutionEnvironmentPath::new(&session.spec.cwd)),
            actions: Vec::new(),
            nesting_depth: 0,
        };
        let resolved = hop_resolver.resolve(&plan, &mut context)?;
        if resolved.0.len() != 1 {
            return Err(format!(
                "hop chain resolution produced {} actions for session '{}', expected exactly 1",
                resolved.0.len(),
                session.metadata.name
            ));
        }
        match resolved.0.into_iter().next() {
            Some(ResolvedAction::Command(args)) => Ok(arg::flatten(&args, 0)),
            Some(other) => {
                Err(format!("hop chain resolution produced a non-command action for session '{}': {other:?}", session.metadata.name))
            }
            None => unreachable!("resolved action count checked above"),
        }
    }
}

pub enum PresentationDeps {
    InSync,
    Applied(AppliedPresentation),
    TornDown { message: Option<String> },
    Failed(String),
    UnknownPolicy(String),
}

impl<R> Reconciler for PresentationReconciler<R>
where
    R: PresentationRuntime + 'static,
{
    type Resource = Presentation;
    type Dependencies = PresentationDeps;

    async fn fetch_dependencies(&self, obj: &ResourceObject<Self::Resource>) -> Result<Self::Dependencies, ResourceError> {
        let listed = self.terminal_sessions.list_matching_labels(&obj.spec.process_selector).await?;
        let mut sessions: Vec<_> = listed.items.into_iter().filter(|session| session.metadata.deletion_timestamp.is_none()).collect();
        sessions.sort_by(|left, right| session_sort_key(left).cmp(&session_sort_key(right)));

        let previous = previous_workspace(obj.status.as_ref());
        if sessions.is_empty() {
            if let Some(previous) = previous {
                self.runtime.tear_down(&previous.presentation_manager, &previous.workspace_ref).await.map_err(ResourceError::other)?;
                return Ok(PresentationDeps::TornDown { message: None });
            }
            if has_any_observed_state(obj.status.as_ref()) {
                return Ok(PresentationDeps::TornDown { message: obj.status.as_ref().and_then(|status| status.message.clone()) });
            }
            return Ok(PresentationDeps::InSync);
        }

        if self.policies.resolve(&obj.spec.presentation_policy_ref).is_none() {
            return Ok(PresentationDeps::UnknownPolicy(obj.spec.presentation_policy_ref.clone()));
        }

        let mut processes = Vec::with_capacity(sessions.len());
        for session in &sessions {
            processes.push(self.resolve_process(session).await.map_err(ResourceError::other)?);
        }
        let spec_hash = presentation_spec_hash(&obj.spec.presentation_policy_ref, &processes);
        if observed_in_sync(obj.status.as_ref(), &spec_hash) {
            return Ok(PresentationDeps::InSync);
        }

        let plan = PresentationPlan {
            policy: obj.spec.presentation_policy_ref.clone(),
            name: obj.spec.name.clone(),
            processes,
            presentation_local_cwd: self.hop_chain.presentation_local_cwd(),
            previous,
            spec_hash,
        };

        // This dependency fetch intentionally performs the runtime apply. The apply result is the
        // authoritative observed workspace state we need to patch into PresentationStatus, and the
        // controller loop processes a given primary object serially through fetch/reconcile/patch.
        Ok(match self.runtime.apply(&plan).await {
            Ok(applied) => PresentationDeps::Applied(applied),
            Err(ApplyPresentationError::UnknownPolicy(name)) => PresentationDeps::UnknownPolicy(name),
            Err(ApplyPresentationError::RetryFromCleanSlate(message)) => PresentationDeps::TornDown { message: Some(message) },
            Err(ApplyPresentationError::Failed(message)) => PresentationDeps::Failed(message),
        })
    }

    fn reconcile(
        &self,
        _obj: &ResourceObject<Self::Resource>,
        deps: &Self::Dependencies,
        now: chrono::DateTime<chrono::Utc>,
    ) -> ReconcileOutcome<Self::Resource> {
        let patch = match deps {
            PresentationDeps::InSync => None,
            PresentationDeps::Applied(applied) => Some(PresentationStatusPatch::MarkActive {
                presentation_manager: applied.presentation_manager.clone(),
                workspace_ref: applied.workspace_ref.clone(),
                spec_hash: applied.spec_hash.clone(),
                ready_at: now,
            }),
            PresentationDeps::TornDown { message } => Some(PresentationStatusPatch::MarkTornDown { message: message.clone() }),
            PresentationDeps::Failed(message) => Some(PresentationStatusPatch::MarkFailed { message: message.clone() }),
            PresentationDeps::UnknownPolicy(name) => {
                Some(PresentationStatusPatch::MarkFailed { message: format!("unknown presentation policy '{name}'") })
            }
        };

        ReconcileOutcome::new(patch)
    }

    async fn run_finalizer(&self, obj: &ResourceObject<Self::Resource>) -> Result<(), ResourceError> {
        if let Some(previous) = previous_workspace(obj.status.as_ref()) {
            self.runtime.tear_down(&previous.presentation_manager, &previous.workspace_ref).await.map_err(ResourceError::other)?;
        }
        Ok(())
    }

    fn finalizer_name(&self) -> Option<&'static str> {
        Some("flotilla.work/presentation-teardown")
    }
}

pub struct ProviderPresentationRuntime {
    registry: Arc<ProviderRegistry>,
    policies: Arc<PresentationPolicyRegistry>,
}

impl ProviderPresentationRuntime {
    pub fn new(registry: Arc<ProviderRegistry>, policies: Arc<PresentationPolicyRegistry>) -> Self {
        Self { registry, policies }
    }
}

#[async_trait]
impl PresentationRuntime for ProviderPresentationRuntime {
    async fn apply(&self, plan: &PresentationPlan) -> Result<AppliedPresentation, ApplyPresentationError> {
        let policy = self.policies.resolve(&plan.policy).ok_or_else(|| ApplyPresentationError::UnknownPolicy(plan.policy.clone()))?;
        let (descriptor, manager) = self
            .registry
            .presentation_managers
            .preferred_with_desc()
            .ok_or_else(|| ApplyPresentationError::Failed("no presentation manager configured".to_string()))?;
        let rendered = policy.render(&plan.processes, &PolicyContext {
            name: plan.name.clone(),
            presentation_local_cwd: plan.presentation_local_cwd.clone(),
        });

        let mut tore_down_previous = false;
        if let Some(previous) = &plan.previous {
            if let Some((_, old_manager)) = self.registry.presentation_managers.get(&previous.presentation_manager) {
                match old_manager.delete_workspace(&previous.workspace_ref).await {
                    Ok(()) => tore_down_previous = true,
                    Err(err) => warn!(
                        manager = %previous.presentation_manager,
                        ws = %previous.workspace_ref,
                        %err,
                        "failed to tear down previous presentation workspace"
                    ),
                }
            } else {
                warn!(
                    manager = %previous.presentation_manager,
                    ws = %previous.workspace_ref,
                    "previous presentation manager unavailable; old workspace may be leaked"
                );
            }
        }

        let (workspace_ref, _) = manager.create_workspace(&rendered.attach_request).await.map_err(|err| {
            if tore_down_previous {
                ApplyPresentationError::RetryFromCleanSlate(err)
            } else {
                ApplyPresentationError::Failed(err)
            }
        })?;

        Ok(AppliedPresentation {
            presentation_manager: descriptor.implementation.clone(),
            workspace_ref,
            spec_hash: plan.spec_hash.clone(),
        })
    }

    async fn tear_down(&self, manager: &str, workspace_ref: &str) -> Result<(), String> {
        let (_, presentation_manager) = self
            .registry
            .presentation_managers
            .get(manager)
            .ok_or_else(|| format!("presentation manager '{manager}' no longer available"))?;
        presentation_manager.delete_workspace(workspace_ref).await
    }
}

fn default_policy_template_yaml(roles: &[String]) -> String {
    let mut yaml = String::from("content:\n");
    for role in roles {
        yaml.push_str("  - role: ");
        yaml.push_str(&yaml_string(role));
        yaml.push('\n');
        yaml.push_str("    type: terminal\n");
        yaml.push_str("    command: \"\"\n");
    }
    yaml.push_str("layout:\n");
    for (index, role) in roles.iter().enumerate() {
        yaml.push_str("  - slot: ");
        yaml.push_str(&yaml_string(role));
        yaml.push('\n');
        if index > 0 {
            yaml.push_str("    split: right\n");
        }
        yaml.push_str(if index == 0 { "    focus: true\n" } else { "    overflow: tab\n" });
    }
    yaml
}

fn yaml_string(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len() + 2);
    escaped.push('"');
    for ch in value.chars() {
        match ch {
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            c if c.is_control() => {
                write!(&mut escaped, "\\u{:04x}", c as u32).expect("writing to string should not fail");
            }
            c => escaped.push(c),
        }
    }
    escaped.push('"');
    escaped
}

fn presentation_spec_hash(policy: &str, processes: &[ResolvedProcess]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(policy.as_bytes());
    hasher.update([0]);
    for process in processes {
        hasher.update(process.role.as_bytes());
        hasher.update([0]);
        hasher.update(process.attach_command.as_bytes());
        hasher.update([0]);
        for (key, value) in &process.labels {
            hasher.update(key.as_bytes());
            hasher.update([0]);
            hasher.update(value.as_bytes());
            hasher.update([0]);
        }
        hasher.update([0xff]);
    }
    format!("{:x}", hasher.finalize())
}

fn previous_workspace(status: Option<&PresentationStatus>) -> Option<PreviousWorkspace> {
    let status = status?;
    Some(PreviousWorkspace {
        presentation_manager: status.observed_presentation_manager.clone()?,
        workspace_ref: status.observed_workspace_ref.clone()?,
    })
}

fn observed_in_sync(status: Option<&PresentationStatus>, spec_hash: &str) -> bool {
    let Some(status) = status else {
        return false;
    };
    status.observed_workspace_ref.is_some()
        && status.observed_presentation_manager.is_some()
        && status.observed_spec_hash.as_deref() == Some(spec_hash)
}

fn has_any_observed_state(status: Option<&PresentationStatus>) -> bool {
    let Some(status) = status else {
        return false;
    };
    status.observed_workspace_ref.is_some() || status.observed_presentation_manager.is_some() || status.observed_spec_hash.is_some()
}

fn session_sort_key(session: &ResourceObject<TerminalSession>) -> (&str, &str, &str) {
    (
        session.metadata.labels.get(flotilla_resources::TASK_ORDINAL_LABEL).map(String::as_str).unwrap_or(""),
        session.metadata.labels.get(flotilla_resources::PROCESS_ORDINAL_LABEL).map(String::as_str).unwrap_or(""),
        session.metadata.name.as_str(),
    )
}

#[cfg(test)]
mod tests {
    use super::yaml_string;

    #[test]
    fn yaml_string_escapes_control_characters() {
        assert_eq!(yaml_string("role\n\r\t\"\\\u{7}"), "\"role\\n\\r\\t\\\"\\\\\\u0007\"");
    }
}
