//! Daemon-side command executor.
//!
//! Takes a `Command`, the repo context, and returns a `CommandValue`.
//! No UI state mutation — all results are carried in the return value.

pub(crate) mod checkout;
mod session_actions;

use std::sync::Arc;

use flotilla_protocol::{
    qualified_path::{PathQualifier, QualifiedPath},
    CheckoutTarget, Command, CommandAction, CommandValue, HostName, NodeId,
};
use tracing::{debug, error, info};

use self::{
    checkout::{checkout_is_local_owned, resolve_checkout_branch, CheckoutIntent, CheckoutResolutionScope, CheckoutService},
    session_actions::ReadOnlySessionActionService,
};
use crate::providers::environment::{legacy_environment_spec, EnvironmentKind};
use crate::{
    data,
    environment_manager::{CreateProvisionedEnvironmentRequest, EnvironmentManager},
    provider_data::ProviderData,
    providers::{
        discovery::EnvVars, issue_tracker::forge_issue_source, registry::ProviderRegistry, vcs::write_branch_issue_links, CommandRunner,
    },
    step::{Step, StepAction, StepExecutionContext, StepOutcome, StepPlan, StepResolver},
};
use flotilla_paths::path_context::{DaemonHostPath, ExecutionEnvironmentPath};

#[derive(Clone)]
pub struct RepoExecutionContext {
    pub identity: flotilla_protocol::RepoIdentity,
    pub root: ExecutionEnvironmentPath,
}

struct CheckoutFlow<'a> {
    branch: &'a str,
    create_branch: bool,
    intent: CheckoutIntent,
    repo_root: &'a ExecutionEnvironmentPath,
    vcs: &'a dyn crate::vcs::Vcs,
    providers_data: &'a ProviderData,
    local_host: &'a HostName,
    new_checkout_qualifier: PathQualifier,
    /// When true, skip host-side validation and de-duplication.
    /// Environment checkouts delegate validation to `ReferenceCloneStrategy`.
    is_environment: bool,
}

impl<'a> CheckoutFlow<'a> {
    fn existing_checkout_path(&self) -> Option<QualifiedPath> {
        if self.is_environment {
            // Environment namespaces are independent — host checkouts don't apply
            return None;
        }
        self.providers_data.checkouts.iter().find_map(|(hp, co)| {
            if checkout_is_local_owned(hp, self.local_host) && co.branch == self.branch {
                Some(hp.clone())
            } else {
                None
            }
        })
    }

    async fn checkout_created_result(&self) -> Result<CommandValue, String> {
        let checkout_service = CheckoutService::new(self.vcs);

        if let Some(path) = self.existing_checkout_path() {
            if matches!(self.intent, CheckoutIntent::FreshBranch) {
                return Err(format!("branch already exists: {}", self.branch));
            }
            return Ok(CommandValue::CheckoutCreated { branch: self.branch.to_string(), path });
        }

        // In environment context, skip host-side branch validation — the
        // ReferenceCloneStrategy validates during clone (git clone -b fails if
        // branch doesn't exist; --no-checkout handles fresh branches).
        if !self.is_environment {
            checkout_service.validate_target(self.repo_root, self.branch, self.intent).await?;
        }

        let path = checkout_service.create_checkout(self.repo_root, self.branch, self.create_branch).await?;
        Ok(CommandValue::CheckoutCreated {
            branch: self.branch.to_string(),
            path: QualifiedPath { qualifier: self.new_checkout_qualifier.clone(), path: path.into_path_buf() },
        })
    }
}

#[derive(Debug)]
pub struct PlannerRefusal {
    message: String,
}

impl PlannerRefusal {
    fn new(message: impl Into<String>) -> Self {
        Self { message: message.into() }
    }

    pub fn into_command_value(self) -> CommandValue {
        CommandValue::Error { message: self.message }
    }

    #[cfg(test)]
    fn message(&self) -> &str {
        &self.message
    }
}

/// Build a step plan for a command.
///
/// Returns `Ok(StepPlan)` for all per-repo commands, or `Err(PlannerRefusal)`
/// for daemon-level commands that should never reach this function and for
/// checkout resolution errors.
#[allow(clippy::too_many_arguments)]
pub async fn build_plan(
    cmd: Command,
    providers_data: Arc<ProviderData>,
    local_node_id: NodeId,
    local_host: HostName,
) -> Result<StepPlan, PlannerRefusal> {
    let Command { node_id, provisioning_target, action, .. } = cmd;
    let target_node_id = node_id.unwrap_or_else(|| local_node_id.clone());
    let checkout_host = StepExecutionContext::Host(target_node_id.clone());
    let target_display_host = provisioning_target.as_ref().map(|target| target.host().clone());

    match action {
        CommandAction::QueryResourceDigest { .. }
        | CommandAction::QueryResourceList { .. }
        | CommandAction::QueryResourceGet { .. }
        | CommandAction::QueryExplainConvoy { .. }
        | CommandAction::ArtifactReserveLedgerComment { .. }
        | CommandAction::ResourceApply { .. }
        | CommandAction::ResourceManifestResolve { .. }
        | CommandAction::ConvoyEnsureRoll { .. }
        | CommandAction::ResourceReconcileNow { .. }
        | CommandAction::MessageFailBatch { .. }
        | CommandAction::ResourceStatusPatch { .. }
        | CommandAction::ResourceDelete { .. }
        | CommandAction::ResourceWatch { .. } => {
            Err(PlannerRefusal::new("resource commands should be handled by the daemon resource dispatcher"))
        }
        CommandAction::Checkout { target, issue_ids, .. } => {
            match provisioning_target {
                Some(flotilla_protocol::ProvisioningTarget::NewEnvironment { provider, .. }) => {
                    return Ok(build_environment_checkout_plan(provider, target, issue_ids, target_node_id));
                }
                Some(flotilla_protocol::ProvisioningTarget::ExistingEnvironment { env_id, .. }) => {
                    return Ok(build_existing_environment_checkout_plan(env_id, target, issue_ids, target_node_id));
                }
                Some(flotilla_protocol::ProvisioningTarget::Host { .. }) | None => {
                    // Fall through to standard checkout
                }
            }
            let (branch, create_branch, intent) = match target {
                CheckoutTarget::Branch(branch) => (branch, false, CheckoutIntent::ExistingBranch),
                CheckoutTarget::FreshBranch(branch) => (branch, true, CheckoutIntent::FreshBranch),
            };
            Ok(build_create_checkout_plan(branch, create_branch, intent, issue_ids, checkout_host))
        }

        CommandAction::RemoveCheckout { checkout } => {
            let checkout_scope = if target_node_id == local_node_id {
                CheckoutResolutionScope::Local
            } else if let Some(target_host) = target_display_host.as_ref() {
                CheckoutResolutionScope::Host(target_host.clone())
            } else {
                CheckoutResolutionScope::RemoteAny
            };
            debug!(
                ?checkout, %target_node_id, %local_host,
                checkout_hosts = ?providers_data.checkouts.keys().map(|qp| (qp.host_name(), &qp.path)).collect::<Vec<_>>(),
                "resolving checkout for removal"
            );
            match resolve_checkout_branch(&checkout, &providers_data, &local_host, &checkout_scope) {
                Ok(branch) => {
                    info!(%branch, %target_node_id, "built remove checkout plan");
                    Ok(build_remove_checkout_plan(branch, target_node_id))
                }
                Err(message) => {
                    error!(%message, %target_node_id, %local_host, "checkout resolution failed");
                    Err(PlannerRefusal::new(message))
                }
            }
        }

        CommandAction::ArchiveSession { session_id } => Ok(build_archive_session_plan(session_id, target_node_id.clone())),

        CommandAction::GenerateBranchName { issue_keys } => Ok(build_generate_branch_name_plan(issue_keys, target_node_id.clone())),

        CommandAction::FetchCheckoutStatus { branch, checkout_path, change_request_id } => Ok(StepPlan::new(vec![Step {
            description: format!("Fetch checkout status for {branch}"),
            host: StepExecutionContext::Host(target_node_id.clone()),
            action: StepAction::FetchCheckoutStatus {
                branch,
                checkout_path: checkout_path.map(ExecutionEnvironmentPath::new),
                change_request_id,
            },
        }])),

        CommandAction::OpenChangeRequest { id } => Ok(StepPlan::new(vec![Step {
            description: format!("Open change request {id}"),
            host: StepExecutionContext::Host(target_node_id.clone()),
            action: StepAction::OpenChangeRequest { id },
        }])),

        CommandAction::CloseChangeRequest { id } => Ok(StepPlan::new(vec![Step {
            description: format!("Close change request {id}"),
            host: StepExecutionContext::Host(target_node_id.clone()),
            action: StepAction::CloseChangeRequest { id },
        }])),

        CommandAction::MergeChangeRequest { id, confirmed } => {
            if !confirmed {
                return Err(PlannerRefusal::new(format!("merging change request {id} requires explicit confirmation")));
            }
            Ok(StepPlan::new(vec![Step {
                description: format!("Merge change request {id}"),
                host: StepExecutionContext::Host(target_node_id.clone()),
                action: StepAction::MergeChangeRequest { id },
            }]))
        }

        CommandAction::OpenIssue { id } => Ok(StepPlan::new(vec![Step {
            description: format!("Open issue {id}"),
            host: StepExecutionContext::Host(target_node_id.clone()),
            action: StepAction::OpenIssue { id },
        }])),

        CommandAction::LinkIssuesToChangeRequest { change_request_id, issue_ids } => Ok(StepPlan::new(vec![Step {
            description: format!("Link issues to change request {change_request_id}"),
            host: StepExecutionContext::Host(target_node_id),
            action: StepAction::LinkIssuesToChangeRequest { change_request_id, issue_ids },
        }])),

        // Daemon-level commands should not reach build_plan.
        CommandAction::ConvoyWorkForceComplete { .. }
        | CommandAction::ConvoyDelete { .. }
        | CommandAction::ConvoyLink { .. }
        | CommandAction::ConvoyUnlink { .. }
        | CommandAction::ConvoyAbandon { .. }
        | CommandAction::ConvoyResume { .. }
        | CommandAction::ConvoyWithdrawPendingBrief { .. }
        | CommandAction::CrewComplete { .. }
        | CommandAction::CrewFail { .. }
        | CommandAction::CrewStall { .. }
        | CommandAction::CrewSupervise { .. }
        | CommandAction::CrewHandoff { .. }
        | CommandAction::ConvoyCreate { .. }
        | CommandAction::ConvoyStart { .. }
        | CommandAction::WorkflowTemplateApply { .. }
        | CommandAction::ProjectAdd { .. }
        | CommandAction::ProjectApply { .. }
        | CommandAction::ProjectRegister { .. }
        | CommandAction::ProjectRefresh { .. }
        | CommandAction::TrackRepoPath { .. }
        | CommandAction::UntrackRepo { .. }
        | CommandAction::RepositoryRemoteRemove { .. }
        | CommandAction::Refresh { .. }
        | CommandAction::QueryResolveRepository { .. }
        | CommandAction::QueryRepoProviders { .. }
        | CommandAction::QueryHostList {}
        | CommandAction::QueryExplainProject { .. }
        | CommandAction::QueryProjectList {}
        | CommandAction::QueryCliList { .. }
        | CommandAction::QueryDispatchBoard { .. }
        | CommandAction::QueryDispatchQueue { .. }
        | CommandAction::QueryFleetHealth {}
        | CommandAction::FleetPostInstall { .. }
        | CommandAction::QueryFulfilmentList {}
        | CommandAction::QueryFleetList { .. }
        | CommandAction::QueryCrewStalls { .. }
        | CommandAction::QueryCrewCapabilities { .. }
        | CommandAction::QueryMessageContacts { .. }
        | CommandAction::QueryCrewList { .. }
        | CommandAction::QueryDaemonLogs { .. }
        | CommandAction::QueryHostStatus { .. }
        | CommandAction::QueryHostProviders { .. }
        | CommandAction::Attach { .. }
        | CommandAction::AttachTransient { .. }
        | CommandAction::QueryIssues { .. }
        | CommandAction::QueryIssueFetchByIds { .. }
        | CommandAction::QueryIssueOpenInBrowser { .. } => Err(PlannerRefusal::new("bug: daemon-level command reached per-repo executor")),
    }
}

/// Build a step plan for `CreateCheckout`.
///
/// Steps:
/// 1. Create the checkout (skipped if it already exists on the local host)
/// 2. Link issues to the branch (skipped if no issue_ids)
///
/// All steps are symbolic — the `ExecutorStepResolver` provides infrastructure
/// (registry, providers_data, runner, local_host) at execution time.
fn build_create_checkout_plan(
    branch: String,
    create_branch: bool,
    intent: CheckoutIntent,
    issue_ids: Vec<(String, String)>,
    checkout_host: StepExecutionContext,
) -> StepPlan {
    let mut steps = Vec::new();

    steps.push(Step {
        description: format!("Create checkout for branch {branch}"),
        host: checkout_host.clone(),
        action: StepAction::CreateCheckout { branch: branch.clone(), create_branch, intent, issue_ids: issue_ids.clone() },
    });

    if !issue_ids.is_empty() {
        steps.push(Step {
            description: "Link issues to branch".to_string(),
            host: checkout_host,
            action: StepAction::LinkIssuesToBranch { branch: branch.clone(), issue_ids },
        });
    }

    StepPlan::new(steps)
}

/// Build a step plan for a new-environment-targeted checkout.
///
/// Steps:
/// 1. ReadEnvironmentSpec on Host(target_host) — reads `.flotilla/environment.yaml`
/// 2. CreateEnvironment on Host(target_host) — resolves and pulls/builds the image
/// 3. CreateCheckout on Environment(target_host, env_id)
fn build_environment_checkout_plan(
    provider: String,
    target: CheckoutTarget,
    issue_ids: Vec<(String, String)>,
    target_node_id: NodeId,
) -> StepPlan {
    let (branch, create_branch, intent) = match target {
        CheckoutTarget::Branch(branch) => (branch, false, CheckoutIntent::ExistingBranch),
        CheckoutTarget::FreshBranch(branch) => (branch, true, CheckoutIntent::FreshBranch),
    };

    let env_id = flotilla_protocol::EnvironmentId::new(uuid::Uuid::new_v4().to_string());
    let host_context = StepExecutionContext::Host(target_node_id.clone());
    let env_context = StepExecutionContext::Environment(target_node_id.clone(), env_id.clone());

    let mut steps = vec![
        Step { description: "Read environment spec".to_string(), host: host_context.clone(), action: StepAction::ReadEnvironmentSpec },
        Step {
            description: format!("Create environment {env_id}"),
            host: host_context.clone(),
            action: StepAction::CreateEnvironment { env_id: env_id.clone(), provider },
        },
        Step {
            description: format!("Create checkout for branch {branch}"),
            host: env_context.clone(),
            action: StepAction::CreateCheckout { branch: branch.clone(), create_branch, intent, issue_ids: issue_ids.clone() },
        },
    ];

    if !issue_ids.is_empty() {
        steps.push(Step {
            description: "Link issues to branch".to_string(),
            host: env_context.clone(),
            action: StepAction::LinkIssuesToBranch { branch: branch.clone(), issue_ids },
        });
    }

    StepPlan::new(steps)
}

/// Build a step plan for attaching to an existing running environment.
///
/// Steps:
/// 1. CreateCheckout on Environment(target_host, env_id)
/// 2. (optional) LinkIssuesToBranch
fn build_existing_environment_checkout_plan(
    env_id: flotilla_protocol::EnvironmentId,
    target: CheckoutTarget,
    issue_ids: Vec<(String, String)>,
    target_node_id: NodeId,
) -> StepPlan {
    let (branch, create_branch, intent) = match target {
        CheckoutTarget::Branch(branch) => (branch, false, CheckoutIntent::ExistingBranch),
        CheckoutTarget::FreshBranch(branch) => (branch, true, CheckoutIntent::FreshBranch),
    };
    let env_context = StepExecutionContext::Environment(target_node_id, env_id);
    let mut steps = vec![Step {
        description: format!("Create checkout for branch {branch}"),
        host: env_context.clone(),
        action: StepAction::CreateCheckout { branch: branch.clone(), create_branch, intent, issue_ids: issue_ids.clone() },
    }];
    if !issue_ids.is_empty() {
        steps.push(Step {
            description: "Link issues to branch".to_string(),
            host: env_context,
            action: StepAction::LinkIssuesToBranch { branch, issue_ids },
        });
    }
    StepPlan::new(steps)
}

/// Build a step plan for `RemoveCheckout`.
///
/// Steps:
/// 1. Remove the checkout via the checkout manager
fn build_remove_checkout_plan(branch: String, local_node_id: NodeId) -> StepPlan {
    StepPlan::new(vec![Step {
        description: format!("Remove checkout for branch {branch}"),
        host: StepExecutionContext::Host(local_node_id),
        action: StepAction::RemoveCheckout { branch },
    }])
}

/// Resolves symbolic `StepAction` variants using executor infrastructure.
pub(crate) struct ExecutorStepResolver {
    pub repo: RepoExecutionContext,
    pub registry: Arc<ProviderRegistry>,
    pub providers_data: Arc<ProviderData>,
    pub runner: Arc<dyn CommandRunner>,
    pub env: Arc<dyn EnvVars>,
    pub config_base: DaemonHostPath,
    pub daemon_socket_path: Option<DaemonHostPath>,
    pub local_host: HostName,
    pub environment_manager: Arc<EnvironmentManager>,
    pub vcs_resolver: Arc<dyn crate::vcs::CheckoutVcsResolver>,
}

#[async_trait::async_trait]
impl StepResolver for ExecutorStepResolver {
    async fn resolve(
        &self,
        _description: &str,
        context: &StepExecutionContext,
        action: StepAction,
        prior: &[StepOutcome],
    ) -> Result<StepOutcome, String> {
        // Part F: Environment-polymorphic dispatch — determine the effective
        // registry, runner, repo root, and providers data based on whether
        // we're inside an environment.
        let (effective_registry, effective_runner, effective_repo_root, effective_providers_data) = match context {
            StepExecutionContext::Host(_) => {
                (self.registry.clone(), self.runner.clone(), self.repo.root.clone(), self.providers_data.clone())
            }
            StepExecutionContext::Environment(_, env_id) => {
                self.environment_manager.ensure_provisioned_environment_providers(env_id, &self.config_base).await?;
                let registry = self
                    .environment_manager
                    .environment_registry(env_id)
                    .ok_or_else(|| format!("environment registry not found: {env_id}"))?;
                let runner =
                    self.environment_manager.environment_runner(env_id).ok_or_else(|| format!("environment handle not found: {env_id}"))?;
                // Interior repo root from prior CreateCheckout outcome, or /workspace
                let repo_root = prior
                    .iter()
                    .find_map(|o| match o {
                        StepOutcome::CompletedWith(CommandValue::CheckoutCreated { path, .. }) => {
                            Some(ExecutionEnvironmentPath::new(&path.path))
                        }
                        _ => None,
                    })
                    .unwrap_or_else(|| ExecutionEnvironmentPath::new("/workspace"));
                // Environments have no pre-existing checkout/provider state from the host
                let providers_data = Arc::new(ProviderData::default());
                (registry, runner, repo_root, providers_data)
            }
        };

        // Extract environment_id from context for use in action handlers
        let context_environment_id = match context {
            StepExecutionContext::Environment(_, env_id) => Some(env_id.clone()),
            _ => None,
        };
        let issue_source = forge_issue_source(&self.repo.identity);

        match action {
            StepAction::CreateCheckout { branch, create_branch, intent, .. } => {
                let vcs = self.vcs_resolver.vcs_for(context_environment_id.as_ref(), effective_repo_root.as_path()).await?;
                let checkout_flow = CheckoutFlow {
                    branch: &branch,
                    create_branch,
                    intent,
                    repo_root: &effective_repo_root,
                    vcs: vcs.as_ref(),
                    providers_data: effective_providers_data.as_ref(),
                    local_host: &self.local_host,
                    new_checkout_qualifier: context_environment_id.as_ref().map_or_else(
                        || PathQualifier::Host(self.environment_manager.local_host_id().clone()),
                        |env_id| PathQualifier::Environment(env_id.clone()),
                    ),
                    is_environment: context_environment_id.is_some(),
                };
                let result = checkout_flow.checkout_created_result().await?;
                if let CommandValue::CheckoutCreated { ref path, .. } = result {
                    info!(checkout_path = %path, "created checkout");
                }
                Ok(StepOutcome::CompletedWith(result))
            }
            StepAction::LinkIssuesToBranch { branch, issue_ids } => {
                write_branch_issue_links(effective_repo_root.as_path(), &branch, &issue_ids, &*effective_runner).await;
                Ok(StepOutcome::Completed)
            }
            StepAction::RemoveCheckout { branch } => {
                let vcs = self.vcs_resolver.vcs_for(context_environment_id.as_ref(), effective_repo_root.as_path()).await?;
                let checkout_service = CheckoutService::new(vcs.as_ref());
                checkout_service.remove_checkout(&self.repo.root, &branch).await?;
                Ok(StepOutcome::CompletedWith(CommandValue::CheckoutRemoved { branch }))
            }

            StepAction::ArchiveSession { session_id } => {
                let session_actions =
                    ReadOnlySessionActionService::new(issue_source.clone(), effective_registry.as_ref(), effective_providers_data.as_ref());
                match session_actions.archive_session_result(&session_id).await {
                    CommandValue::Error { message } => Err(message),
                    result => Ok(StepOutcome::CompletedWith(result)),
                }
            }
            StepAction::GenerateBranchName { issue_keys } => {
                let session_actions =
                    ReadOnlySessionActionService::new(issue_source.clone(), effective_registry.as_ref(), effective_providers_data.as_ref());
                Ok(StepOutcome::CompletedWith(session_actions.generate_branch_name_result(&issue_keys).await))
            }
            StepAction::FetchCheckoutStatus { branch, checkout_path, change_request_id } => {
                let vcs_path = checkout_path.as_ref().map_or(effective_repo_root.as_path(), |path| path.as_path());
                let vcs = self.vcs_resolver.vcs_for(context_environment_id.as_ref(), vcs_path).await?;
                let info = data::fetch_checkout_status(
                    &branch,
                    checkout_path.as_ref().map(|p| p.as_path()),
                    change_request_id.as_deref(),
                    effective_repo_root.as_path(),
                    vcs.as_ref(),
                    effective_runner.as_ref(),
                )
                .await;
                Ok(StepOutcome::CompletedWith(CommandValue::CheckoutStatus(Box::new(info))))
            }
            StepAction::OpenChangeRequest { id } => {
                debug!(%id, "opening change request in browser");
                if let Some(cr) = self.registry.change_requests.preferred() {
                    let _ = cr.open_in_browser(&id).await;
                }
                Ok(StepOutcome::Completed)
            }
            StepAction::CloseChangeRequest { id } => {
                debug!(%id, "closing change request");
                if let Some(cr) = self.registry.change_requests.preferred() {
                    let _ = cr.close_change_request(&id).await;
                }
                Ok(StepOutcome::Completed)
            }
            StepAction::MergeChangeRequest { id } => {
                debug!(%id, "merging change request");
                let cr = self
                    .registry
                    .change_requests
                    .preferred()
                    .ok_or_else(|| "no change request provider is active for this repository".to_string())?;
                cr.merge_change_request(&id).await?;
                Ok(StepOutcome::Completed)
            }
            StepAction::OpenIssue { id } => {
                debug!(%id, "opening issue in browser");
                if let Some(provider) = self.registry.issue_provider_for(&issue_source) {
                    let _ = provider.open_in_browser(&flotilla_protocol::IssueRef { source: issue_source.clone(), id }).await;
                }
                Ok(StepOutcome::Completed)
            }
            StepAction::LinkIssuesToChangeRequest { change_request_id, issue_ids } => {
                info!(issue_ids = ?issue_ids, %change_request_id, "linking issues to change request");
                let provider =
                    self.registry.change_requests.preferred().ok_or("no change request provider is active for this Repository")?;
                provider.link_issues(&change_request_id, &issue_ids).await?;
                Ok(StepOutcome::Completed)
            }

            // -----------------------------------------------------------------
            // Environment lifecycle actions — always use host-side providers
            // -----------------------------------------------------------------
            StepAction::ReadEnvironmentSpec => {
                let vcs = self.vcs_resolver.vcs_for(None, self.repo.root.as_path()).await?;
                let yaml = vcs
                    .read_repository(
                        self.repo.root.as_path(),
                        crate::vcs::RepositoryRead::FileAtRevision("HEAD:.flotilla/environment.yaml"),
                    )
                    .await
                    .map_err(|e| format!("failed to read .flotilla/environment.yaml from HEAD: {e}"))?;
                let spec: flotilla_protocol::EnvironmentSpec =
                    serde_yml::from_str(&yaml).map_err(|e| format!("invalid .flotilla/environment.yaml: {e}"))?;
                Ok(StepOutcome::Produced(CommandValue::EnvironmentSpecRead { spec }))
            }
            StepAction::CreateEnvironment { env_id, provider } => {
                let spec = prior
                    .iter()
                    .find_map(|outcome| match outcome {
                        StepOutcome::Produced(CommandValue::EnvironmentSpecRead { spec }) => Some(spec),
                        _ => None,
                    })
                    .ok_or_else(|| "spec not produced by prior ReadEnvironmentSpec step".to_string())?;
                let (_, env_provider) = self
                    .registry
                    .environment_providers
                    .select(EnvironmentKind::Docker, Some(&provider))
                    .ok_or_else(|| format!("environment provider not available: {provider}"))?;
                let resource_spec = legacy_environment_spec(spec)?;
                // TODO(#2862 Part B): mint an operation-scoped RegistryAuth here;
                // anonymous preparation must use an empty adapter configuration.
                let prepared = env_provider.prepare(&resource_spec, &Default::default()).await?;
                let tokens = spec
                    .token_env_vars
                    .iter()
                    .filter_map(|name| match self.env.get(name) {
                        Some(val) => Some((name.clone(), val)),
                        None => {
                            tracing::warn!(env_var = %name, "token env var not set on host, skipping");
                            None
                        }
                    })
                    .collect();
                let reference_repo = self.resolve_reference_repo().await;
                let daemon_socket =
                    self.daemon_socket_path.clone().ok_or_else(|| "daemon socket path required for environment creation".to_string())?;
                self.environment_manager
                    .create_provisioned_environment(CreateProvisionedEnvironmentRequest {
                        env_id: env_id.clone(),
                        provider: &provider,
                        registry: self.registry.as_ref(),
                        prepared,
                        tokens,
                        config_base: &self.config_base,
                        daemon_socket_path: &daemon_socket,
                        reference_repo,
                    })
                    .await?;
                Ok(StepOutcome::Completed)
            }
            StepAction::DestroyEnvironment { env_id } => {
                self.environment_manager.destroy_provisioned_environment(&env_id).await?;
                Ok(StepOutcome::Completed)
            }

            StepAction::Noop => Ok(StepOutcome::Completed),
        }
    }
}

impl ExecutorStepResolver {
    // TODO: reference repo resolution is provider-specific (Docker needs a host-side
    // path to bind-mount). This should move into the EnvironmentProvider or CreateOpts
    // preparation rather than living on the executor.
    async fn resolve_reference_repo(&self) -> Option<DaemonHostPath> {
        let vcs = self.vcs_resolver.vcs_for(None, self.repo.root.as_path()).await.ok()?;
        let result = vcs.read_repository(self.repo.root.as_path(), crate::vcs::RepositoryRead::SharedMetadataDir).await;
        match result {
            Ok(path) => {
                let git_dir = std::path::Path::new(path.trim());
                // git returns a relative path (relative to cwd); DaemonHostPath requires absolute.
                let abs = if git_dir.is_relative() { self.repo.root.as_path().join(git_dir) } else { git_dir.to_path_buf() };
                Some(DaemonHostPath::new(abs))
            }
            Err(_) => None,
        }
    }
}

fn build_archive_session_plan(session_id: String, local_node_id: NodeId) -> StepPlan {
    StepPlan::new(vec![Step {
        description: format!("Archive session {session_id}"),
        host: StepExecutionContext::Host(local_node_id),
        action: StepAction::ArchiveSession { session_id },
    }])
}

fn build_generate_branch_name_plan(issue_keys: Vec<String>, local_node_id: NodeId) -> StepPlan {
    StepPlan::new(vec![Step {
        description: "Generate branch name".to_string(),
        host: StepExecutionContext::Host(local_node_id),
        action: StepAction::GenerateBranchName { issue_keys },
    }])
}

#[cfg(test)]
mod tests;
