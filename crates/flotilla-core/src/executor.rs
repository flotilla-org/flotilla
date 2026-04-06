//! Daemon-side command executor.
//!
//! Takes a `Command`, the repo context, and returns a `CommandValue`.
//! No UI state mutation — all results are carried in the return value.

pub(crate) mod checkout;
mod session_actions;
mod terminals;
mod workspace;

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use flotilla_protocol::{
    arg::Arg, qualified_path::QualifiedPath, CheckoutTarget, Command, CommandAction, CommandValue, HostName, PreparedWorkspace,
    ResolvedPaneCommand,
};
use tracing::{debug, error, info};

use self::{
    checkout::{checkout_is_local_owned, resolve_checkout_branch, CheckoutIntent, CheckoutService},
    session_actions::{resolve_attach_command, ReadOnlySessionActionService, TeleportFlow, TeleportSessionActionService},
    terminals::TerminalPreparationService,
    workspace::WorkspaceOrchestrator,
};
use crate::{
    attachable::SharedAttachableStore,
    data,
    environment_manager::{CreateProvisionedEnvironmentRequest, EnvironmentManager},
    path_context::{DaemonHostPath, ExecutionEnvironmentPath},
    provider_data::ProviderData,
    providers::{
        discovery::EnvVars, registry::ProviderRegistry, run, types::WorkspaceConfig, vcs::write_branch_issue_links, CommandRunner,
    },
    step::{Step, StepAction, StepExecutionContext, StepOutcome, StepPlan, StepResolver},
    terminal_manager::TerminalManager,
};

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
    registry: &'a ProviderRegistry,
    providers_data: &'a ProviderData,
    local_host: &'a HostName,
    /// When true, skip host-side validation and de-duplication.
    /// Environment checkouts delegate validation to `CloneCheckoutManager`.
    is_environment: bool,
}

impl<'a> CheckoutFlow<'a> {
    fn existing_checkout_path(&self) -> Option<ExecutionEnvironmentPath> {
        if self.is_environment {
            // Environment namespaces are independent — host checkouts don't apply
            return None;
        }
        self.providers_data.checkouts.iter().find_map(|(hp, co)| {
            if checkout_is_local_owned(hp, self.local_host) && co.branch == self.branch {
                Some(ExecutionEnvironmentPath::new(&hp.path))
            } else {
                None
            }
        })
    }

    async fn checkout_created_result(&self) -> Result<CommandValue, String> {
        let checkout_service = CheckoutService::new(self.registry);

        if let Some(path) = self.existing_checkout_path() {
            if matches!(self.intent, CheckoutIntent::FreshBranch) {
                return Err(format!("branch already exists: {}", self.branch));
            }
            return Ok(CommandValue::CheckoutCreated { branch: self.branch.to_string(), path: path.into_path_buf() });
        }

        // In environment context, skip host-side branch validation — the
        // CloneCheckoutManager validates during clone (git clone -b fails if
        // branch doesn't exist; --no-checkout handles fresh branches).
        if !self.is_environment {
            checkout_service.validate_target(self.repo_root, self.branch, self.intent).await?;
        }

        let path = checkout_service.create_checkout(self.repo_root, self.branch, self.create_branch).await?;
        Ok(CommandValue::CheckoutCreated { branch: self.branch.to_string(), path: path.into_path_buf() })
    }
}

/// Build a step plan for a command.
///
/// Returns `Ok(StepPlan)` for all per-repo commands, or `Err(CommandValue)`
/// for daemon-level commands that should never reach this function and for
/// pre-resolution errors (e.g. teleport with an unknown checkout key).
#[allow(clippy::too_many_arguments)]
pub async fn build_plan(
    cmd: Command,
    repo: RepoExecutionContext,
    registry: Arc<ProviderRegistry>,
    providers_data: Arc<ProviderData>,
    config_base: DaemonHostPath,
    attachable_store: SharedAttachableStore,
    daemon_socket_path: Option<DaemonHostPath>,
    local_host: HostName,
) -> Result<StepPlan, CommandValue> {
    let Command { host, provisioning_target, action, .. } = cmd;
    let target_host = host.unwrap_or_else(|| local_host.clone());
    let checkout_host = StepExecutionContext::Host(target_host.clone());

    match action {
        CommandAction::Checkout { target, issue_ids, .. } => {
            match provisioning_target {
                Some(flotilla_protocol::ProvisioningTarget::NewEnvironment { provider, .. }) => {
                    return Ok(build_environment_checkout_plan(provider, target, issue_ids, target_host, local_host));
                }
                Some(flotilla_protocol::ProvisioningTarget::ExistingEnvironment { env_id, .. }) => {
                    return Ok(build_existing_environment_checkout_plan(env_id, target, issue_ids, target_host, local_host));
                }
                Some(flotilla_protocol::ProvisioningTarget::Host { .. }) | None => {
                    // Fall through to standard checkout
                }
            }
            let (branch, create_branch, intent) = match target {
                CheckoutTarget::Branch(branch) => (branch, false, CheckoutIntent::ExistingBranch),
                CheckoutTarget::FreshBranch(branch) => (branch, true, CheckoutIntent::FreshBranch),
            };
            Ok(build_create_checkout_plan(branch, create_branch, intent, issue_ids, checkout_host, local_host))
        }

        CommandAction::TeleportSession { session_id, branch, checkout_key } => {
            build_teleport_session_plan(
                session_id,
                branch,
                checkout_key,
                repo.root,
                registry,
                providers_data,
                config_base,
                attachable_store.clone(),
                daemon_socket_path.clone(),
                local_host,
            )
            .await
        }

        CommandAction::RemoveCheckout { checkout } => {
            debug!(
                ?checkout, %target_host, %local_host,
                checkout_hosts = ?providers_data.checkouts.keys().map(|qp| (qp.host_name(), &qp.path)).collect::<Vec<_>>(),
                "resolving checkout for removal"
            );
            match resolve_checkout_branch(&checkout, &providers_data, &target_host) {
                Ok(branch) => {
                    let deleted_paths: Vec<QualifiedPath> = providers_data
                        .checkouts
                        .iter()
                        .filter(|(qp, co)| co.branch == branch && checkout_is_local_owned(qp, &target_host))
                        .map(|(qp, _)| qp.clone())
                        .collect();
                    info!(%branch, ?deleted_paths, %target_host, "built remove checkout plan");
                    Ok(build_remove_checkout_plan(branch, deleted_paths, target_host))
                }
                Err(message) => {
                    error!(%message, %target_host, %local_host, "checkout resolution failed");
                    Err(CommandValue::Error { message })
                }
            }
        }

        CommandAction::ArchiveSession { session_id } => Ok(build_archive_session_plan(session_id, local_host)),

        CommandAction::GenerateBranchName { issue_keys } => Ok(build_generate_branch_name_plan(issue_keys, local_host)),

        CommandAction::CreateWorkspaceForCheckout { checkout_path, label } => Ok(build_create_workspace_plan(
            workspace_label_for_host(&label, &checkout_host, &local_host),
            Some(ExecutionEnvironmentPath::new(checkout_path)),
            checkout_host,
            local_host,
        )),

        CommandAction::CreateWorkspaceFromPreparedTerminal { target_host, branch, checkout_path, attachable_set_id, commands } => {
            Ok(StepPlan::new(vec![Step {
                description: format!("Create workspace from prepared terminal for {branch}"),
                host: StepExecutionContext::Host(local_host),
                action: StepAction::CreateWorkspaceFromPreparedTerminal {
                    target_host,
                    branch,
                    checkout_path: ExecutionEnvironmentPath::new(checkout_path),
                    attachable_set_id,
                    commands,
                },
            }]))
        }

        CommandAction::SelectWorkspace { ws_ref } => Ok(StepPlan::new(vec![Step {
            description: format!("Select workspace {ws_ref}"),
            host: StepExecutionContext::Host(local_host),
            action: StepAction::SelectWorkspace { ws_ref },
        }])),

        CommandAction::PrepareTerminalForCheckout { checkout_path, commands } => Ok(StepPlan::new(vec![Step {
            description: "Prepare terminal for checkout".to_string(),
            host: checkout_host,
            action: StepAction::PrepareTerminalForCheckout { checkout_path: ExecutionEnvironmentPath::new(checkout_path), commands },
        }])),

        CommandAction::FetchCheckoutStatus { branch, checkout_path, change_request_id } => Ok(StepPlan::new(vec![Step {
            description: format!("Fetch checkout status for {branch}"),
            host: StepExecutionContext::Host(local_host),
            action: StepAction::FetchCheckoutStatus {
                branch,
                checkout_path: checkout_path.map(ExecutionEnvironmentPath::new),
                change_request_id,
            },
        }])),

        CommandAction::OpenChangeRequest { id } => Ok(StepPlan::new(vec![Step {
            description: format!("Open change request {id}"),
            host: StepExecutionContext::Host(local_host),
            action: StepAction::OpenChangeRequest { id },
        }])),

        CommandAction::CloseChangeRequest { id } => Ok(StepPlan::new(vec![Step {
            description: format!("Close change request {id}"),
            host: StepExecutionContext::Host(local_host),
            action: StepAction::CloseChangeRequest { id },
        }])),

        CommandAction::OpenIssue { id } => Ok(StepPlan::new(vec![Step {
            description: format!("Open issue {id}"),
            host: StepExecutionContext::Host(local_host),
            action: StepAction::OpenIssue { id },
        }])),

        CommandAction::LinkIssuesToChangeRequest { change_request_id, issue_ids } => Ok(StepPlan::new(vec![Step {
            description: format!("Link issues to change request {change_request_id}"),
            host: StepExecutionContext::Host(local_host),
            action: StepAction::LinkIssuesToChangeRequest { change_request_id, issue_ids },
        }])),

        // Daemon-level commands should not reach build_plan.
        CommandAction::TrackRepoPath { .. }
        | CommandAction::UntrackRepo { .. }
        | CommandAction::Refresh { .. }
        | CommandAction::QueryRepoDetail { .. }
        | CommandAction::QueryRepoProviders { .. }
        | CommandAction::QueryRepoWork { .. }
        | CommandAction::QueryHostList {}
        | CommandAction::QueryHostStatus { .. }
        | CommandAction::QueryHostProviders { .. }
        | CommandAction::QueryIssues { .. }
        | CommandAction::QueryIssueFetchByIds { .. }
        | CommandAction::QueryIssueOpenInBrowser { .. } => {
            Err(CommandValue::Error { message: "bug: daemon-level command reached per-repo executor".to_string() })
        }
    }
}

/// Build a step plan for `CreateCheckout`.
///
/// Steps:
/// 1. Create the checkout (skipped if it already exists on the local host)
/// 2. Link issues to the branch (skipped if no issue_ids)
/// 3. Create a workspace for the new checkout
///
/// All steps are symbolic — the `ExecutorStepResolver` provides infrastructure
/// (registry, providers_data, runner, local_host) at execution time.
fn build_create_checkout_plan(
    branch: String,
    create_branch: bool,
    intent: CheckoutIntent,
    issue_ids: Vec<(String, String)>,
    checkout_host: StepExecutionContext,
    local_host: HostName,
) -> StepPlan {
    let mut steps = Vec::new();
    let workspace_host = checkout_host.clone();

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

    let workspace_label = workspace_label_for_host(&branch, &workspace_host, &local_host);
    steps.extend(build_create_workspace_plan(workspace_label, None, workspace_host, local_host).steps);

    StepPlan::new(steps)
}

/// Build a step plan for a new-environment-targeted checkout.
///
/// Steps:
/// 1. ReadEnvironmentSpec on Host(target_host) — reads `.flotilla/environment.yaml`
/// 2. EnsureEnvironmentImage on Host(target_host) — resolves spec from prior step
/// 3. CreateEnvironment on Host(target_host)
/// 4. CreateCheckout on Environment(target_host, env_id)
/// 5. PrepareWorkspace on Environment(target_host, env_id)
/// 6. AttachWorkspace on Host(local_host)
fn build_environment_checkout_plan(
    provider: String,
    target: CheckoutTarget,
    issue_ids: Vec<(String, String)>,
    target_host: HostName,
    local_host: HostName,
) -> StepPlan {
    let (branch, create_branch, intent) = match target {
        CheckoutTarget::Branch(branch) => (branch, false, CheckoutIntent::ExistingBranch),
        CheckoutTarget::FreshBranch(branch) => (branch, true, CheckoutIntent::FreshBranch),
    };

    let env_id = flotilla_protocol::EnvironmentId::new(uuid::Uuid::new_v4().to_string());
    let host_context = StepExecutionContext::Host(target_host.clone());
    let env_context = StepExecutionContext::Environment(target_host.clone(), env_id.clone());

    let mut steps = vec![
        Step { description: "Read environment spec".to_string(), host: host_context.clone(), action: StepAction::ReadEnvironmentSpec },
        Step {
            description: "Ensure environment image".to_string(),
            host: host_context.clone(),
            action: StepAction::EnsureEnvironmentImage { provider: provider.clone() },
        },
        Step {
            description: format!("Create environment {env_id}"),
            host: host_context.clone(),
            action: StepAction::CreateEnvironment { env_id: env_id.clone(), provider, image: None },
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

    let workspace_label = if target_host == local_host { branch.clone() } else { format!("{branch}@{target_host}") };

    steps.push(Step {
        description: format!("Prepare workspace for {workspace_label}"),
        host: env_context,
        action: StepAction::PrepareWorkspace { checkout_path: None, label: workspace_label },
    });

    steps.push(Step {
        description: "Attach workspace".to_string(),
        host: StepExecutionContext::Host(local_host),
        action: StepAction::AttachWorkspace,
    });

    StepPlan::new(steps)
}

/// Build a step plan for attaching to an existing running environment.
///
/// Steps:
/// 1. CreateCheckout on Environment(target_host, env_id)
/// 2. (optional) LinkIssuesToBranch
/// 3. PrepareWorkspace on Environment(target_host, env_id)
/// 4. AttachWorkspace on Host(local_host)
fn build_existing_environment_checkout_plan(
    env_id: flotilla_protocol::EnvironmentId,
    target: CheckoutTarget,
    issue_ids: Vec<(String, String)>,
    target_host: HostName,
    local_host: HostName,
) -> StepPlan {
    let (branch, create_branch, intent) = match target {
        CheckoutTarget::Branch(branch) => (branch, false, CheckoutIntent::ExistingBranch),
        CheckoutTarget::FreshBranch(branch) => (branch, true, CheckoutIntent::FreshBranch),
    };
    let env_context = StepExecutionContext::Environment(target_host.clone(), env_id.clone());
    let workspace_label = if target_host == local_host { branch.clone() } else { format!("{branch}@{target_host}") };

    let mut steps = vec![Step {
        description: format!("Create checkout for branch {branch}"),
        host: env_context.clone(),
        action: StepAction::CreateCheckout { branch: branch.clone(), create_branch, intent, issue_ids: issue_ids.clone() },
    }];

    if !issue_ids.is_empty() {
        steps.push(Step {
            description: "Link issues to branch".to_string(),
            host: env_context.clone(),
            action: StepAction::LinkIssuesToBranch { branch: branch.clone(), issue_ids },
        });
    }

    steps.push(Step {
        description: format!("Prepare workspace for {workspace_label}"),
        host: env_context,
        action: StepAction::PrepareWorkspace { checkout_path: None, label: workspace_label },
    });
    steps.push(Step {
        description: "Attach workspace".to_string(),
        host: StepExecutionContext::Host(local_host),
        action: StepAction::AttachWorkspace,
    });

    StepPlan::new(steps)
}

fn build_create_workspace_plan(
    label: String,
    checkout_path: Option<ExecutionEnvironmentPath>,
    checkout_host: StepExecutionContext,
    local_host: HostName,
) -> StepPlan {
    StepPlan::new(vec![
        Step {
            description: format!("Prepare workspace for {label}"),
            host: checkout_host,
            action: StepAction::PrepareWorkspace { checkout_path, label },
        },
        Step {
            description: "Attach workspace".to_string(),
            host: StepExecutionContext::Host(local_host),
            action: StepAction::AttachWorkspace,
        },
    ])
}

fn workspace_label_for_host(label: &str, host: &StepExecutionContext, local_host: &HostName) -> String {
    let target = host.host_name();
    if *target == *local_host {
        label.to_string()
    } else {
        format!("{label}@{target}")
    }
}

/// Build a step plan for `TeleportSession`.
///
/// Steps:
/// 1. Resolve attach command from the session's cloud agent provider
/// 2. Ensure checkout exists (skipped if checkout_key references a known checkout, or no branch)
/// 3. Create workspace with the teleport (attach) command
#[allow(clippy::too_many_arguments)]
async fn build_teleport_session_plan(
    session_id: String,
    branch: Option<String>,
    checkout_key: Option<PathBuf>,
    repo_root: ExecutionEnvironmentPath,
    registry: Arc<ProviderRegistry>,
    providers_data: Arc<ProviderData>,
    config_base: DaemonHostPath,
    attachable_store: SharedAttachableStore,
    daemon_socket_path: Option<DaemonHostPath>,
    local_host: flotilla_protocol::HostName,
) -> Result<StepPlan, CommandValue> {
    let checkout_key_ee = checkout_key.map(ExecutionEnvironmentPath::new);
    let teleport_flow = TeleportFlow::new(
        &repo_root,
        registry.as_ref(),
        providers_data.as_ref(),
        &config_base,
        &attachable_store,
        daemon_socket_path.as_ref().map(|p| p.as_path()),
        &local_host,
        &session_id,
        branch.as_deref(),
        checkout_key_ee.as_ref(),
    );
    let initial_path = match teleport_flow.initial_checkout_path().await {
        Ok(path) => path,
        Err(message) => return Err(CommandValue::Error { message }),
    };

    let steps = vec![
        Step {
            description: format!("Resolve attach command for session {session_id}"),
            host: StepExecutionContext::Host(local_host.clone()),
            action: StepAction::ResolveAttachCommand { session_id: session_id.clone() },
        },
        Step {
            description: "Ensure checkout for teleport".to_string(),
            host: StepExecutionContext::Host(local_host.clone()),
            action: StepAction::EnsureCheckoutForTeleport { branch: branch.clone(), checkout_key: checkout_key_ee, initial_path },
        },
        Step {
            description: "Create workspace with teleport command".to_string(),
            host: StepExecutionContext::Host(local_host),
            action: StepAction::CreateTeleportWorkspace { session_id, branch },
        },
    ];

    Ok(StepPlan::new(steps))
}

/// Build a step plan for `RemoveCheckout`.
///
/// Steps:
/// 1. Remove the checkout via the checkout manager
/// 2. Clean up correlated terminal sessions (best-effort)
fn build_remove_checkout_plan(branch: String, deleted_checkout_paths: Vec<QualifiedPath>, local_host: HostName) -> StepPlan {
    StepPlan::new(vec![Step {
        description: format!("Remove checkout for branch {branch}"),
        host: StepExecutionContext::Host(local_host),
        action: StepAction::RemoveCheckout { branch, deleted_checkout_paths },
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
    pub attachable_store: SharedAttachableStore,
    pub daemon_socket_path: Option<DaemonHostPath>,
    pub local_host: HostName,
    pub environment_manager: Arc<EnvironmentManager>,
}

impl ExecutorStepResolver {
    /// Construct a `TerminalManager` from the registry's preferred terminal pool, if one exists.
    fn terminal_manager(&self) -> Option<TerminalManager> {
        self.registry
            .terminal_pools
            .preferred()
            .map(|pool| TerminalManager::new(Arc::clone(pool), self.attachable_store.clone(), self.local_host.clone()))
    }
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
                        StepOutcome::CompletedWith(CommandValue::CheckoutCreated { path, .. }) => Some(ExecutionEnvironmentPath::new(path)),
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

        match action {
            StepAction::CreateCheckout { branch, create_branch, intent, .. } => {
                let checkout_flow = CheckoutFlow {
                    branch: &branch,
                    create_branch,
                    intent,
                    repo_root: &effective_repo_root,
                    registry: effective_registry.as_ref(),
                    providers_data: effective_providers_data.as_ref(),
                    local_host: &self.local_host,
                    is_environment: context_environment_id.is_some(),
                };
                let result = checkout_flow.checkout_created_result().await?;
                if let CommandValue::CheckoutCreated { ref path, .. } = result {
                    info!(checkout_path = %path.display(), "created checkout");
                }
                Ok(StepOutcome::CompletedWith(result))
            }
            StepAction::LinkIssuesToBranch { branch, issue_ids } => {
                write_branch_issue_links(effective_repo_root.as_path(), &branch, &issue_ids, &*effective_runner).await;
                Ok(StepOutcome::Completed)
            }
            StepAction::RemoveCheckout { branch, deleted_checkout_paths } => {
                let checkout_service = CheckoutService::new(effective_registry.as_ref());
                let tm = self.terminal_manager();
                checkout_service.remove_checkout(&self.repo.root, &branch, &deleted_checkout_paths, tm.as_ref()).await?;
                Ok(StepOutcome::CompletedWith(CommandValue::CheckoutRemoved { branch }))
            }

            StepAction::ResolveAttachCommand { session_id } => {
                let cmd = resolve_attach_command(&session_id, self.registry.as_ref(), self.providers_data.as_ref()).await?;
                Ok(StepOutcome::Produced(CommandValue::AttachCommandResolved { command: cmd }))
            }
            StepAction::EnsureCheckoutForTeleport { branch, checkout_key, initial_path } => {
                if let Some(path) = initial_path {
                    return Ok(StepOutcome::Produced(CommandValue::CheckoutPathResolved { path: path.into_path_buf() }));
                }
                let tm = self.terminal_manager();
                let service = TeleportSessionActionService::new(
                    &self.repo.root,
                    self.registry.as_ref(),
                    self.providers_data.as_ref(),
                    &self.config_base,
                    &self.attachable_store,
                    self.daemon_socket_path.as_ref().map(|p| p.as_path()),
                    &self.local_host,
                    tm.as_ref(),
                );
                match service.resolve_teleport_checkout_path(checkout_key.as_ref(), branch.as_deref()).await? {
                    Some(path) => Ok(StepOutcome::Produced(CommandValue::CheckoutPathResolved { path: path.into_path_buf() })),
                    None => Ok(StepOutcome::Skipped),
                }
            }
            StepAction::CreateTeleportWorkspace { session_id: _, branch } => {
                let cmd = prior
                    .iter()
                    .find_map(|o| match o {
                        StepOutcome::Produced(CommandValue::AttachCommandResolved { command }) => Some(command.clone()),
                        _ => None,
                    })
                    .ok_or_else(|| "attach command not resolved by prior step".to_string())?;

                let path = prior
                    .iter()
                    .find_map(|o| match o {
                        StepOutcome::Produced(CommandValue::CheckoutPathResolved { path }) => Some(path.clone()),
                        _ => None,
                    })
                    .ok_or_else(|| "checkout path not resolved by prior step".to_string())?;

                let tm = self.terminal_manager();
                let service = TeleportSessionActionService::new(
                    &self.repo.root,
                    self.registry.as_ref(),
                    self.providers_data.as_ref(),
                    &self.config_base,
                    &self.attachable_store,
                    self.daemon_socket_path.as_ref().map(|p| p.as_path()),
                    &self.local_host,
                    tm.as_ref(),
                );
                service.create_workspace_for_teleport(&path, branch.as_deref(), &cmd).await?;
                Ok(StepOutcome::Completed)
            }
            StepAction::ArchiveSession { session_id } => {
                let session_actions = ReadOnlySessionActionService::new(self.registry.as_ref(), self.providers_data.as_ref());
                match session_actions.archive_session_result(&session_id).await {
                    CommandValue::Error { message } => Err(message),
                    result => Ok(StepOutcome::CompletedWith(result)),
                }
            }
            StepAction::GenerateBranchName { issue_keys } => {
                let session_actions = ReadOnlySessionActionService::new(self.registry.as_ref(), self.providers_data.as_ref());
                Ok(StepOutcome::CompletedWith(session_actions.generate_branch_name_result(&issue_keys).await))
            }
            StepAction::PrepareWorkspace { checkout_path: explicit_path, label } => {
                let prepared_checkout: Option<(ExecutionEnvironmentPath, String, Option<QualifiedPath>)> = if let Some(p) = explicit_path {
                    let host_key =
                        flotilla_protocol::qualified_path::QualifiedPath::from_host_name(&self.local_host, p.as_path().to_path_buf());
                    let (checkout_key, branch) = self
                        .providers_data
                        .checkouts
                        .get_key_value(&host_key)
                        .or_else(|| {
                            self.providers_data
                                .checkouts
                                .iter()
                                .find(|(hp, _)| checkout_is_local_owned(hp, &self.local_host) && hp.path == p.as_path())
                        })
                        .map(|(path, checkout)| (path.clone(), checkout.branch.clone()))
                        .ok_or_else(|| format!("checkout not found: {}", p))?;
                    Some((p, branch, Some(checkout_key)))
                } else {
                    prior.iter().find_map(|o| match o {
                        StepOutcome::CompletedWith(CommandValue::CheckoutCreated { branch, path }) => {
                            Some((ExecutionEnvironmentPath::new(path), branch.clone(), None))
                        }
                        _ => None,
                    })
                };

                let Some((checkout_path, branch, checkout_key)) = prepared_checkout else {
                    return Ok(StepOutcome::Skipped);
                };

                let tm = self.terminal_manager();
                let workspace_orchestrator = WorkspaceOrchestrator::new(
                    self.repo.root.as_path(),
                    effective_registry.as_ref(),
                    self.config_base.as_path(),
                    &self.attachable_store,
                    self.daemon_socket_path.as_ref().map(|p| p.as_path()),
                    &self.local_host,
                    tm.as_ref(),
                );
                let attachable_set_id = workspace_orchestrator.ensure_attachable_set_for_checkout(
                    &self.local_host,
                    checkout_path.as_path(),
                    checkout_key.as_ref(),
                    context_environment_id.as_ref(),
                );
                let workspace_config =
                    workspace_config(self.repo.root.as_path(), &label, checkout_path.as_path(), "claude", self.config_base.as_path());
                let template_yaml = workspace_config.template_yaml.clone();
                let prepared_commands = if let Some(ref tm) = tm {
                    let terminal_preparation = TerminalPreparationService::new(tm, self.daemon_socket_path.as_ref().map(|p| p.as_path()));
                    let workspace_config = workspace_config.clone();
                    terminal_preparation
                        .prepare_terminal_commands(&branch, checkout_path.as_path(), &[], move || workspace_config.clone())
                        .await?
                } else {
                    let workspace_config = workspace_config.clone();
                    terminals::render_fallback_commands(move || workspace_config.clone())
                        .into_iter()
                        .map(|cmd| ResolvedPaneCommand { role: cmd.role, args: vec![Arg::Literal(cmd.command)] })
                        .collect()
                };

                let container_name =
                    context_environment_id.as_ref().and_then(|env_id| self.environment_manager.environment_container_name(env_id));

                Ok(StepOutcome::Produced(CommandValue::PreparedWorkspace(PreparedWorkspace {
                    label,
                    target_host: self.local_host.clone(),
                    checkout_path: checkout_path.into_path_buf(),
                    checkout_key,
                    attachable_set_id,
                    environment_id: context_environment_id.clone(),
                    container_name,
                    template_yaml,
                    prepared_commands,
                })))
            }
            StepAction::AttachWorkspace => {
                let prepared = prior
                    .iter()
                    .rev()
                    .find_map(|o| match o {
                        StepOutcome::Produced(CommandValue::PreparedWorkspace(prepared)) => Some(prepared.clone()),
                        _ => None,
                    })
                    .ok_or_else(|| "prepared workspace not produced by prior step".to_string())?;

                // container_name flows through the PreparedWorkspace payload from
                // the remote daemon — no local handle lookup needed.
                let container_name = prepared.container_name.clone();

                let tm = self.terminal_manager();
                let workspace_orchestrator = WorkspaceOrchestrator::new(
                    self.repo.root.as_path(),
                    self.registry.as_ref(),
                    self.config_base.as_path(),
                    &self.attachable_store,
                    self.daemon_socket_path.as_ref().map(|p| p.as_path()),
                    &self.local_host,
                    tm.as_ref(),
                );
                workspace_orchestrator.attach_prepared_workspace(&prepared, container_name.as_deref()).await?;
                Ok(StepOutcome::Completed)
            }
            StepAction::CreateWorkspaceFromPreparedTerminal { target_host, branch, checkout_path, attachable_set_id, commands } => {
                let tm = self.terminal_manager();
                let workspace_orchestrator = WorkspaceOrchestrator::new(
                    self.repo.root.as_path(),
                    self.registry.as_ref(),
                    self.config_base.as_path(),
                    &self.attachable_store,
                    self.daemon_socket_path.as_ref().map(|p| p.as_path()),
                    &self.local_host,
                    tm.as_ref(),
                );
                workspace_orchestrator
                    .attach_prepared_workspace(
                        &PreparedWorkspace {
                            label: format!("{branch}@{target_host}"),
                            target_host,
                            checkout_path: checkout_path.into_path_buf(),
                            checkout_key: None,
                            attachable_set_id,
                            environment_id: None,
                            container_name: None,
                            template_yaml: None,
                            prepared_commands: commands,
                        },
                        None,
                    )
                    .await?;
                Ok(StepOutcome::Completed)
            }
            StepAction::SelectWorkspace { ws_ref } => {
                info!(%ws_ref, "switching to workspace");
                let tm = self.terminal_manager();
                let workspace_orchestrator = WorkspaceOrchestrator::new(
                    self.repo.root.as_path(),
                    self.registry.as_ref(),
                    self.config_base.as_path(),
                    &self.attachable_store,
                    self.daemon_socket_path.as_ref().map(|p| p.as_path()),
                    &self.local_host,
                    tm.as_ref(),
                );
                workspace_orchestrator.select_workspace(&ws_ref).await?;
                Ok(StepOutcome::Completed)
            }
            StepAction::PrepareTerminalForCheckout { checkout_path, commands: requested_commands } => {
                let host_key = flotilla_protocol::qualified_path::QualifiedPath::from_host_name(
                    &self.local_host,
                    checkout_path.as_path().to_path_buf(),
                );
                let Some((checkout_key, co)) = self
                    .providers_data
                    .checkouts
                    .get_key_value(&host_key)
                    .or_else(|| {
                        self.providers_data
                            .checkouts
                            .iter()
                            .find(|(hp, _)| checkout_is_local_owned(hp, &self.local_host) && hp.path == checkout_path.as_path())
                    })
                    .map(|(path, checkout)| (path.clone(), checkout.clone()))
                else {
                    return Err(format!("checkout not found: {}", checkout_path));
                };

                {
                    let tm = self.terminal_manager();
                    let workspace_orchestrator = WorkspaceOrchestrator::new(
                        self.repo.root.as_path(),
                        self.registry.as_ref(),
                        self.config_base.as_path(),
                        &self.attachable_store,
                        self.daemon_socket_path.as_ref().map(|p| p.as_path()),
                        &self.local_host,
                        tm.as_ref(),
                    );
                    let attachable_set_id = workspace_orchestrator.ensure_attachable_set_for_checkout(
                        &self.local_host,
                        checkout_path.as_path(),
                        Some(&checkout_key),
                        None,
                    );
                    let commands = if let Some(ref tm) = tm {
                        let terminal_preparation =
                            TerminalPreparationService::new(tm, self.daemon_socket_path.as_ref().map(|p| p.as_path()));
                        terminal_preparation
                            .prepare_terminal_commands(&co.branch, checkout_path.as_path(), &requested_commands, || {
                                workspace_config(
                                    self.repo.root.as_path(),
                                    &co.branch,
                                    checkout_path.as_path(),
                                    "claude",
                                    self.config_base.as_path(),
                                )
                            })
                            .await?
                    } else if !requested_commands.is_empty() {
                        requested_commands
                            .iter()
                            .map(|cmd| ResolvedPaneCommand { role: cmd.role.clone(), args: vec![Arg::Literal(cmd.command.clone())] })
                            .collect()
                    } else {
                        terminals::render_fallback_commands(|| {
                            workspace_config(
                                self.repo.root.as_path(),
                                &co.branch,
                                checkout_path.as_path(),
                                "claude",
                                self.config_base.as_path(),
                            )
                        })
                        .into_iter()
                        .map(|cmd| ResolvedPaneCommand { role: cmd.role, args: vec![Arg::Literal(cmd.command)] })
                        .collect()
                    };
                    Ok(StepOutcome::CompletedWith(CommandValue::TerminalPrepared {
                        repo_identity: self.repo.identity.clone(),
                        target_host: self.local_host.clone(),
                        branch: co.branch,
                        checkout_path: checkout_path.into_path_buf(),
                        attachable_set_id,
                        commands,
                    }))
                }
            }
            StepAction::FetchCheckoutStatus { branch, checkout_path, change_request_id } => {
                let info = data::fetch_checkout_status(
                    &branch,
                    checkout_path.as_ref().map(|p| p.as_path()),
                    change_request_id.as_deref(),
                    self.repo.root.as_path(),
                    effective_runner.as_ref(),
                )
                .await;
                Ok(StepOutcome::CompletedWith(CommandValue::CheckoutStatus(info)))
            }
            StepAction::OpenChangeRequest { id } => {
                debug!(%id, "opening change request in browser");
                if let Some(cr) = self.registry.change_requests.preferred() {
                    let _ = cr.open_in_browser(self.repo.root.as_path(), &id).await;
                }
                Ok(StepOutcome::Completed)
            }
            StepAction::CloseChangeRequest { id } => {
                debug!(%id, "closing change request");
                if let Some(cr) = self.registry.change_requests.preferred() {
                    let _ = cr.close_change_request(self.repo.root.as_path(), &id).await;
                }
                Ok(StepOutcome::Completed)
            }
            StepAction::OpenIssue { id } => {
                debug!(%id, "opening issue in browser");
                if let Some(it) = self.registry.issue_trackers.preferred() {
                    let _ = it.open_in_browser(self.repo.root.as_path(), &id).await;
                }
                Ok(StepOutcome::Completed)
            }
            StepAction::LinkIssuesToChangeRequest { change_request_id, issue_ids } => {
                info!(issue_ids = ?issue_ids, %change_request_id, "linking issues to change request");
                let body_result = run!(
                    self.runner.as_ref(),
                    "gh",
                    &["pr", "view", &change_request_id, "--json", "body", "--jq", ".body"],
                    self.repo.root.as_path()
                );
                match body_result {
                    Ok(current_body) => {
                        let fixes_lines: Vec<String> = issue_ids.iter().map(|id| format!("Fixes #{id}")).collect();
                        let new_body = if current_body.trim().is_empty() {
                            fixes_lines.join("\n")
                        } else {
                            format!("{}\n\n{}", current_body.trim(), fixes_lines.join("\n"))
                        };
                        let result = run!(
                            self.runner.as_ref(),
                            "gh",
                            &["pr", "edit", &change_request_id, "--body", &new_body],
                            self.repo.root.as_path()
                        );
                        match result {
                            Ok(_) => {
                                info!(%change_request_id, "linked issues to change request");
                                Ok(StepOutcome::Completed)
                            }
                            Err(e) => {
                                error!(err = %e, "failed to edit change request");
                                Err(e)
                            }
                        }
                    }
                    Err(e) => {
                        error!(err = %e, "failed to read change request body");
                        Err(e)
                    }
                }
            }

            // -----------------------------------------------------------------
            // Environment lifecycle actions — always use host-side providers
            // -----------------------------------------------------------------
            StepAction::ReadEnvironmentSpec => {
                let yaml = self
                    .runner
                    .run(
                        "git",
                        &["show", "HEAD:.flotilla/environment.yaml"],
                        self.repo.root.as_path(),
                        &crate::providers::ChannelLabel::Noop,
                    )
                    .await
                    .map_err(|e| format!("failed to read .flotilla/environment.yaml from HEAD: {e}"))?;
                let spec: flotilla_protocol::EnvironmentSpec =
                    serde_yml::from_str(&yaml).map_err(|e| format!("invalid .flotilla/environment.yaml: {e}"))?;
                Ok(StepOutcome::Produced(CommandValue::EnvironmentSpecRead { spec }))
            }
            StepAction::EnsureEnvironmentImage { provider } => {
                // Extract spec from prior ReadEnvironmentSpec outcome
                let spec = prior
                    .iter()
                    .find_map(|o| match o {
                        StepOutcome::Produced(CommandValue::EnvironmentSpecRead { spec }) => Some(spec.clone()),
                        _ => None,
                    })
                    .ok_or_else(|| "spec not produced by prior ReadEnvironmentSpec step".to_string())?;
                let (_, env_provider) = self
                    .registry
                    .environment_providers
                    .get(&provider)
                    .ok_or_else(|| format!("environment provider not available: {provider}"))?;
                let image = env_provider.ensure_image(&spec, self.repo.root.as_path()).await?;
                Ok(StepOutcome::Produced(CommandValue::ImageEnsured { image }))
            }
            StepAction::CreateEnvironment { env_id, provider, image: _ } => {
                let image = prior
                    .iter()
                    .find_map(|outcome| match outcome {
                        StepOutcome::Produced(CommandValue::ImageEnsured { image }) => Some(image.clone()),
                        _ => None,
                    })
                    .ok_or_else(|| "image not produced by prior EnsureEnvironmentImage step".to_string())?;
                let tokens = prior
                    .iter()
                    .find_map(|outcome| match outcome {
                        StepOutcome::Produced(CommandValue::EnvironmentSpecRead { spec }) => Some(spec),
                        _ => None,
                    })
                    .map(|spec| {
                        spec.token_env_vars
                            .iter()
                            .filter_map(|name| match self.env.get(name) {
                                Some(val) => Some((name.clone(), val)),
                                None => {
                                    tracing::warn!(env_var = %name, "token env var not set on host, skipping");
                                    None
                                }
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                let reference_repo = self.resolve_reference_repo().await;
                let daemon_socket =
                    self.daemon_socket_path.clone().ok_or_else(|| "daemon socket path required for environment creation".to_string())?;
                self.environment_manager
                    .create_provisioned_environment(CreateProvisionedEnvironmentRequest {
                        env_id: env_id.clone(),
                        provider: &provider,
                        registry: self.registry.as_ref(),
                        image,
                        tokens,
                        config_base: &self.config_base,
                        daemon_socket_path: &daemon_socket,
                        reference_repo,
                    })
                    .await?;
                Ok(StepOutcome::Produced(CommandValue::EnvironmentCreated { env_id }))
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
        let result = self
            .runner
            .run("git", &["rev-parse", "--git-common-dir"], self.repo.root.as_path(), &crate::providers::ChannelLabel::Noop)
            .await;
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

fn build_archive_session_plan(session_id: String, local_host: HostName) -> StepPlan {
    StepPlan::new(vec![Step {
        description: format!("Archive session {session_id}"),
        host: StepExecutionContext::Host(local_host),
        action: StepAction::ArchiveSession { session_id },
    }])
}

fn build_generate_branch_name_plan(issue_keys: Vec<String>, local_host: HostName) -> StepPlan {
    StepPlan::new(vec![Step {
        description: "Generate branch name".to_string(),
        host: StepExecutionContext::Host(local_host),
        action: StepAction::GenerateBranchName { issue_keys },
    }])
}
/// Build a WorkspaceConfig from repo/branch/dir/command.
///
/// NOTE: Template search crosses the daemon/execution boundary — reads
/// `.flotilla/workspace.yaml` from `repo_root` (execution environment) and
/// falls back to `config_base.join("workspace.yaml")` (daemon host).
pub(crate) fn workspace_config(
    repo_root: &Path,
    name: &str,
    working_dir: &Path,
    main_command: &str,
    config_base: &Path,
) -> WorkspaceConfig {
    let tmpl_path = repo_root.join(".flotilla/workspace.yaml");
    let template_yaml = std::fs::read_to_string(&tmpl_path).ok().or_else(|| {
        let global_path = config_base.join("workspace.yaml");
        std::fs::read_to_string(global_path).ok()
    });
    let mut template_vars = std::collections::HashMap::new();
    template_vars.insert("main_command".to_string(), main_command.to_string());
    WorkspaceConfig {
        name: name.to_string(),
        working_directory: ExecutionEnvironmentPath::new(working_dir),
        template_vars,
        template_yaml,
        resolved_commands: None,
    }
}

#[cfg(test)]
mod tests;
