//! Daemon-side command executor.
//!
//! Takes a `Command`, the repo context, and returns a `CommandResult`.
//! No UI state mutation — all results are carried in the return value.

mod checkout;
mod terminals;
mod workspace;

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

#[cfg(test)]
use flotilla_protocol::CheckoutSelector;
use flotilla_protocol::{
    CheckoutTarget, Command, CommandAction, CommandResult, HostName, HostPath, ManagedTerminalId, PreparedTerminalCommand,
};
use tracing::{debug, error, info, warn};

#[cfg(test)]
use self::checkout::validate_checkout_target;
#[cfg(test)]
use self::terminals::{build_terminal_env_vars, escape_for_double_quotes, resolve_terminal_pool, wrap_remote_attach_commands};
use self::{
    checkout::{resolve_checkout_branch, write_branch_issue_links, CheckoutIntent, CheckoutService},
    terminals::TerminalPreparationService,
    workspace::WorkspaceOrchestrator,
};
use crate::{
    attachable::SharedAttachableStore,
    data,
    provider_data::ProviderData,
    providers::{
        registry::ProviderRegistry,
        run,
        types::{CloudAgentSession, CorrelationKey, WorkspaceConfig},
        CommandRunner,
    },
    step::{Step, StepAction, StepHost, StepOutcome, StepPlan, StepResolver},
};

/// The result of `build_plan`: either an immediate result or a multi-step plan.
pub enum ExecutionPlan {
    /// Command completed synchronously — no steps needed.
    Immediate(CommandResult),
    /// Command requires multiple steps with cancellation support.
    Steps(StepPlan),
}

#[derive(Clone)]
pub struct RepoExecutionContext {
    pub identity: flotilla_protocol::RepoIdentity,
    pub root: PathBuf,
}

/// Build an execution plan for a command.
///
/// Multi-step commands (CreateCheckout, TeleportSession, RemoveCheckout,
/// ArchiveSession, GenerateBranchName) return `ExecutionPlan::Steps` with
/// cancellation points between steps. All other commands delegate to
/// `execute()` and return `ExecutionPlan::Immediate`.
#[allow(clippy::too_many_arguments)]
pub async fn build_plan(
    cmd: Command,
    repo: RepoExecutionContext,
    registry: Arc<ProviderRegistry>,
    providers_data: Arc<ProviderData>,
    runner: Arc<dyn CommandRunner>,
    config_base: PathBuf,
    attachable_store: SharedAttachableStore,
    daemon_socket_path: Option<PathBuf>,
    local_host: HostName,
    // TODO(multi-host): When a command is forwarded from another host, this carries
    // the requester's hostname so plan builders can stamp `StepHost::Remote(originator)`
    // on steps that need to run back on the presentation host (e.g. workspace creation
    // after a remote checkout). Passed by `execute_forwarded_command` in server.rs.
    _originating_host: Option<HostName>,
) -> ExecutionPlan {
    let Command { action, .. } = cmd;

    match action {
        CommandAction::Checkout { target, issue_ids, .. } => {
            let (branch, create_branch, intent) = match target {
                CheckoutTarget::Branch(branch) => (branch, false, CheckoutIntent::ExistingBranch),
                CheckoutTarget::FreshBranch(branch) => (branch, true, CheckoutIntent::FreshBranch),
            };
            build_create_checkout_plan(branch, create_branch, intent, issue_ids, repo.root, registry, providers_data, runner, local_host)
                .await
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

        CommandAction::RemoveCheckout { checkout, terminal_keys } => {
            match resolve_checkout_branch(&checkout, &providers_data, &local_host) {
                Ok(branch) => build_remove_checkout_plan(branch, terminal_keys, repo.root, registry, runner),
                Err(message) => ExecutionPlan::Immediate(CommandResult::Error { message }),
            }
        }

        CommandAction::ArchiveSession { session_id } => build_archive_session_plan(session_id, registry, providers_data).await,

        CommandAction::GenerateBranchName { issue_keys } => build_generate_branch_name_plan(issue_keys, registry, providers_data).await,

        action => {
            let result = execute(
                action,
                &repo,
                &registry,
                &providers_data,
                &*runner,
                &config_base,
                &attachable_store,
                daemon_socket_path.as_deref(),
                &local_host,
            )
            .await;
            ExecutionPlan::Immediate(result)
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
/// The final step creates a workspace for the new checkout. This is a symbolic
/// step resolved by the `ExecutorStepResolver` at execution time, so it has
/// access to the registry and config without needing pre-refreshed provider data.
#[allow(clippy::too_many_arguments)]
async fn build_create_checkout_plan(
    branch: String,
    create_branch: bool,
    intent: CheckoutIntent,
    issue_ids: Vec<(String, String)>,
    repo_root: PathBuf,
    registry: Arc<ProviderRegistry>,
    providers_data: Arc<ProviderData>,
    runner: Arc<dyn CommandRunner>,
    local_host: flotilla_protocol::HostName,
) -> ExecutionPlan {
    // Check if checkout already exists for this branch on the local host.
    let existing_checkout_path: Option<PathBuf> =
        providers_data.checkouts.iter().find_map(
            |(hp, co)| {
                if hp.host == local_host && co.branch == branch {
                    Some(hp.path.clone())
                } else {
                    None
                }
            },
        );

    let mut steps = Vec::new();

    // Step 1: Create checkout
    {
        let branch = branch.clone();
        let repo_root = repo_root.clone();
        let registry = Arc::clone(&registry);
        let runner = Arc::clone(&runner);
        let existing = existing_checkout_path.clone();
        steps.push(Step {
            description: format!("Create checkout for branch {branch}"),
            host: StepHost::Local,
            action: StepAction::Closure(Box::new(move |_prior| {
                Box::pin(async move {
                    let checkout_service = CheckoutService::new(registry.as_ref(), runner.as_ref());
                    checkout_service.validate_target(&repo_root, &branch, intent).await?;
                    // If checkout already exists, emit CheckoutCreated so the workspace
                    // step can find the path in prior outcomes.
                    if let Some(path) = existing {
                        if matches!(intent, CheckoutIntent::FreshBranch) {
                            return Err(format!("branch already exists: {branch}"));
                        }
                        return Ok(StepOutcome::CompletedWith(CommandResult::CheckoutCreated { branch, path }));
                    }
                    let path = checkout_service.create_checkout(&repo_root, &branch, create_branch).await?;
                    info!(checkout_path = %path.display(), "created checkout");
                    Ok(StepOutcome::CompletedWith(CommandResult::CheckoutCreated { branch, path }))
                })
            })),
        });
    }

    // Step 2: Link issues (only if non-empty)
    if !issue_ids.is_empty() {
        let branch = branch.clone();
        let repo_root = repo_root.clone();
        let runner = Arc::clone(&runner);
        steps.push(Step {
            description: "Link issues to branch".to_string(),
            host: StepHost::Local,
            action: StepAction::Closure(Box::new(move |_prior| {
                Box::pin(async move {
                    write_branch_issue_links(&repo_root, &branch, &issue_ids, &*runner).await;
                    Ok(StepOutcome::Completed)
                })
            })),
        });
    }

    // Final step: create workspace for the new checkout.
    steps.push(Step {
        description: "Create workspace".to_string(),
        host: StepHost::Local,
        action: StepAction::CreateWorkspaceForCheckout { label: branch.clone() },
    });

    ExecutionPlan::Steps(StepPlan::new(steps))
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
    repo_root: PathBuf,
    registry: Arc<ProviderRegistry>,
    providers_data: Arc<ProviderData>,
    config_base: PathBuf,
    attachable_store: SharedAttachableStore,
    daemon_socket_path: Option<PathBuf>,
    local_host: flotilla_protocol::HostName,
) -> ExecutionPlan {
    // Shared slot for the teleport (attach) command — populated by step 1.
    let teleport_cmd_slot: Arc<tokio::sync::Mutex<Option<String>>> = Arc::new(tokio::sync::Mutex::new(None));

    // Shared slot for checkout path — pre-populated if checkout_key references a known checkout.
    let checkout_path_slot: Arc<tokio::sync::Mutex<Option<PathBuf>>> = {
        let existing = checkout_key.as_ref().and_then(|key| {
            let host_key = flotilla_protocol::HostPath::new(local_host.clone(), key.clone());
            providers_data.checkouts.get(&host_key).map(|_| key.clone())
        });
        Arc::new(tokio::sync::Mutex::new(existing))
    };

    let mut steps = Vec::new();

    // Step 1: Resolve attach command
    {
        let slot = Arc::clone(&teleport_cmd_slot);
        let session_id = session_id.clone();
        let registry = Arc::clone(&registry);
        let providers_data = Arc::clone(&providers_data);
        steps.push(Step {
            description: format!("Resolve attach command for session {session_id}"),
            host: StepHost::Local,
            action: StepAction::Closure(Box::new(move |_prior| {
                Box::pin(async move {
                    let cmd = resolve_attach_command(&session_id, &registry, &providers_data).await?;
                    *slot.lock().await = Some(cmd);
                    Ok(StepOutcome::Completed)
                })
            })),
        });
    }

    // Step 2: Ensure checkout if needed
    // Only runs when there's no pre-existing checkout and a branch is provided.
    {
        let slot = Arc::clone(&checkout_path_slot);
        let branch = branch.clone();
        let repo_root = repo_root.clone();
        let registry = Arc::clone(&registry);
        steps.push(Step {
            description: "Ensure checkout for teleport".to_string(),
            host: StepHost::Local,
            action: StepAction::Closure(Box::new(move |_prior| {
                Box::pin(async move {
                    // Already have a checkout path — skip
                    if slot.lock().await.is_some() {
                        return Ok(StepOutcome::Skipped);
                    }
                    let branch_name = match &branch {
                        Some(b) => b.clone(),
                        None => return Ok(StepOutcome::Skipped),
                    };
                    let cm = registry.checkout_managers.preferred().cloned().ok_or_else(|| "No checkout manager available".to_string())?;
                    let (path, _checkout) = cm.create_checkout(&repo_root, &branch_name, false).await?;
                    *slot.lock().await = Some(path);
                    Ok(StepOutcome::Completed)
                })
            })),
        });
    }

    // Step 3: Create workspace with teleport command
    {
        let teleport_slot = Arc::clone(&teleport_cmd_slot);
        let path_slot = Arc::clone(&checkout_path_slot);
        let branch = branch.clone();
        let repo_root = repo_root.clone();
        let registry = Arc::clone(&registry);
        let config_base = config_base.clone();
        let attachable_store = attachable_store.clone();
        let daemon_socket_path = daemon_socket_path.clone();
        let local_host = local_host.clone();
        steps.push(Step {
            description: "Create workspace with teleport command".to_string(),
            host: StepHost::Local,
            action: StepAction::Closure(Box::new(move |_prior| {
                Box::pin(async move {
                    let path =
                        path_slot.lock().await.clone().ok_or_else(|| "Could not determine checkout path for teleport".to_string())?;
                    let teleport_cmd = teleport_slot.lock().await.clone().ok_or_else(|| "Attach command not resolved".to_string())?;
                    let name = branch.as_deref().unwrap_or("session");
                    let workspace_orchestrator = WorkspaceOrchestrator::new(
                        &repo_root,
                        registry.as_ref(),
                        &config_base,
                        &attachable_store,
                        daemon_socket_path.as_deref(),
                        &local_host,
                    );
                    workspace_orchestrator.create_workspace_for_teleport(&path, name, &teleport_cmd).await?;
                    Ok(StepOutcome::Completed)
                })
            })),
        });
    }

    ExecutionPlan::Steps(StepPlan::new(steps))
}

/// Build a step plan for `RemoveCheckout`.
///
/// Steps:
/// 1. Remove the checkout via the checkout manager
/// 2. Clean up correlated terminal sessions (best-effort)
fn build_remove_checkout_plan(
    branch: String,
    terminal_keys: Vec<ManagedTerminalId>,
    repo_root: PathBuf,
    registry: Arc<ProviderRegistry>,
    runner: Arc<dyn CommandRunner>,
) -> ExecutionPlan {
    ExecutionPlan::Steps(StepPlan::new(vec![Step {
        description: format!("Remove checkout for branch {branch}"),
        host: StepHost::Local,
        action: StepAction::Closure(Box::new(move |_prior| {
            Box::pin(async move {
                let checkout_service = CheckoutService::new(registry.as_ref(), runner.as_ref());
                checkout_service.remove_checkout(&repo_root, &branch, &terminal_keys).await?;
                Ok(StepOutcome::Completed)
            })
        })),
    }]))
}

/// Resolves symbolic `StepAction` variants using executor infrastructure.
pub(crate) struct ExecutorStepResolver {
    pub repo: RepoExecutionContext,
    pub registry: Arc<ProviderRegistry>,
    pub config_base: PathBuf,
    pub attachable_store: SharedAttachableStore,
    pub daemon_socket_path: Option<PathBuf>,
    pub local_host: HostName,
}

#[async_trait::async_trait]
impl StepResolver for ExecutorStepResolver {
    async fn resolve(&self, _description: &str, action: StepAction, prior: &[StepOutcome]) -> Result<StepOutcome, String> {
        match action {
            StepAction::Closure(_) => unreachable!("closures handled by stepper directly"),
            StepAction::CreateWorkspaceForCheckout { label } => {
                let path = prior.iter().find_map(|o| match o {
                    StepOutcome::CompletedWith(CommandResult::CheckoutCreated { path, .. }) => Some(path.clone()),
                    _ => None,
                });
                match path {
                    Some(p) => {
                        let workspace_orchestrator = WorkspaceOrchestrator::new(
                            &self.repo.root,
                            self.registry.as_ref(),
                            &self.config_base,
                            &self.attachable_store,
                            self.daemon_socket_path.as_deref(),
                            &self.local_host,
                        );
                        workspace_orchestrator.create_workspace_for_checkout(&p, &label).await
                    }
                    None => Ok(StepOutcome::Skipped),
                }
            }
        }
    }
}

async fn build_archive_session_plan(
    session_id: String,
    registry: Arc<ProviderRegistry>,
    providers_data: Arc<ProviderData>,
) -> ExecutionPlan {
    let should_run_as_step = providers_data
        .sessions
        .get(session_id.as_str())
        .and_then(|session| session_provider_key(session, &session_id))
        .and_then(|provider_key| registry.cloud_agents.get(provider_key))
        .is_some();

    if !should_run_as_step {
        return ExecutionPlan::Immediate(archive_session_result(&session_id, &registry, &providers_data).await);
    }

    ExecutionPlan::Steps(StepPlan::new(vec![Step {
        description: format!("Archive session {session_id}"),
        host: StepHost::Local,
        action: StepAction::Closure(Box::new(move |_prior| {
            Box::pin(async move {
                match archive_session_result(&session_id, &registry, &providers_data).await {
                    CommandResult::Error { message } => Err(message),
                    result => Ok(StepOutcome::CompletedWith(result)),
                }
            })
        })),
    }]))
}

async fn build_generate_branch_name_plan(
    issue_keys: Vec<String>,
    registry: Arc<ProviderRegistry>,
    providers_data: Arc<ProviderData>,
) -> ExecutionPlan {
    if registry.ai_utilities.is_empty() {
        return ExecutionPlan::Immediate(generate_branch_name_result(&issue_keys, &registry, &providers_data).await);
    }

    ExecutionPlan::Steps(StepPlan::new(vec![Step {
        description: "Generate branch name".to_string(),
        host: StepHost::Local,
        action: StepAction::Closure(Box::new(move |_prior| {
            Box::pin(async move {
                match generate_branch_name_result(&issue_keys, &registry, &providers_data).await {
                    CommandResult::Error { message } => Err(message),
                    result => Ok(StepOutcome::CompletedWith(result)),
                }
            })
        })),
    }]))
}
/// Execute a `Command` against the given repo context.
///
/// Commands that are handled at the daemon level (TrackRepoPath, UntrackRepo, Refresh)
/// should not reach this function — the caller should handle them directly.
#[allow(clippy::too_many_arguments)]
pub async fn execute(
    action: CommandAction,
    repo: &RepoExecutionContext,
    registry: &ProviderRegistry,
    providers_data: &ProviderData,
    runner: &dyn CommandRunner,
    config_base: &Path,
    attachable_store: &SharedAttachableStore,
    daemon_socket_path: Option<&Path>,
    local_host: &HostName,
) -> CommandResult {
    match action {
        CommandAction::CreateWorkspaceForCheckout { checkout_path, label } => {
            let host_key = HostPath::new(local_host.clone(), checkout_path.clone());
            if !providers_data.checkouts.contains_key(&host_key) {
                return CommandResult::Error { message: format!("checkout not found: {}", checkout_path.display()) };
            }
            info!(%label, "entering workspace");
            let workspace_orchestrator =
                WorkspaceOrchestrator::new(&repo.root, registry, config_base, attachable_store, daemon_socket_path, local_host);
            match workspace_orchestrator.create_workspace_for_checkout(&checkout_path, &label).await {
                Ok(_) => CommandResult::Ok,
                Err(e) => CommandResult::Error { message: e },
            }
        }

        CommandAction::CreateWorkspaceFromPreparedTerminal { target_host, branch, checkout_path, attachable_set_id, commands } => {
            let workspace_orchestrator =
                WorkspaceOrchestrator::new(&repo.root, registry, config_base, attachable_store, daemon_socket_path, local_host);
            if let Err(message) = workspace_orchestrator
                .create_workspace_from_prepared_terminal(&target_host, &branch, &checkout_path, attachable_set_id.as_ref(), &commands)
                .await
            {
                return CommandResult::Error { message };
            }
            CommandResult::Ok
        }

        CommandAction::SelectWorkspace { ws_ref } => {
            info!(%ws_ref, "switching to workspace");
            let workspace_orchestrator =
                WorkspaceOrchestrator::new(&repo.root, registry, config_base, attachable_store, daemon_socket_path, local_host);
            if let Err(message) = workspace_orchestrator.select_workspace(&ws_ref).await {
                return CommandResult::Error { message };
            }
            CommandResult::Ok
        }

        CommandAction::Checkout { target, issue_ids, .. } => {
            let (branch, create_branch, intent) = match target {
                CheckoutTarget::Branch(branch) => (branch, false, CheckoutIntent::ExistingBranch),
                CheckoutTarget::FreshBranch(branch) => (branch, true, CheckoutIntent::FreshBranch),
            };
            let checkout_service = CheckoutService::new(registry, runner);
            if let Err(message) = checkout_service.validate_target(&repo.root, &branch, intent).await {
                return CommandResult::Error { message };
            }
            info!(%branch, "creating checkout");
            match checkout_service.create_checkout(&repo.root, &branch, create_branch).await {
                Ok(checkout_path) => {
                    // Write issue links to git config
                    if !issue_ids.is_empty() {
                        checkout_service.write_branch_issue_links(&repo.root, &branch, &issue_ids).await;
                    }
                    info!(checkout_path = %checkout_path.display(), "created checkout");
                    CommandResult::CheckoutCreated { branch: branch.clone(), path: checkout_path }
                }
                Err(e) => {
                    error!(err = %e, "create checkout failed");
                    CommandResult::Error { message: e }
                }
            }
        }

        CommandAction::PrepareTerminalForCheckout { checkout_path, commands: requested_commands } => {
            let host_key = HostPath::new(local_host.clone(), checkout_path.clone());
            if let Some(co) = providers_data.checkouts.get(&host_key).cloned() {
                let workspace_orchestrator =
                    WorkspaceOrchestrator::new(&repo.root, registry, config_base, attachable_store, daemon_socket_path, local_host);
                let attachable_set_id = workspace_orchestrator.ensure_attachable_set_for_checkout(local_host, &checkout_path);
                let terminal_preparation = TerminalPreparationService::new(registry, config_base, attachable_store, daemon_socket_path);
                match terminal_preparation
                    .prepare_terminal_commands(&co.branch, &checkout_path, &requested_commands, || {
                        workspace_config(&repo.root, &co.branch, &checkout_path, "claude", config_base)
                    })
                    .await
                {
                    Ok(commands) => CommandResult::TerminalPrepared {
                        repo_identity: repo.identity.clone(),
                        target_host: local_host.clone(),
                        branch: co.branch,
                        checkout_path,
                        attachable_set_id,
                        commands,
                    },
                    Err(message) => CommandResult::Error { message },
                }
            } else {
                CommandResult::Error { message: format!("checkout not found: {}", checkout_path.display()) }
            }
        }

        CommandAction::RemoveCheckout { checkout, terminal_keys } => {
            let checkout_service = CheckoutService::new(registry, runner);
            let branch = match resolve_checkout_branch(&checkout, providers_data, local_host) {
                Ok(branch) => branch,
                Err(message) => return CommandResult::Error { message },
            };
            info!(%branch, "removing checkout");
            match checkout_service.remove_checkout(&repo.root, &branch, &terminal_keys).await {
                Ok(()) => CommandResult::CheckoutRemoved { branch },
                Err(e) => CommandResult::Error { message: e },
            }
        }

        CommandAction::FetchCheckoutStatus { branch, checkout_path, change_request_id } => {
            let info =
                data::fetch_checkout_status(&branch, checkout_path.as_deref(), change_request_id.as_deref(), &repo.root, runner).await;
            CommandResult::CheckoutStatus(info)
        }

        CommandAction::OpenChangeRequest { id } => {
            debug!(%id, "opening change request in browser");
            if let Some(cr) = registry.change_requests.preferred() {
                let _ = cr.open_in_browser(&repo.root, &id).await;
            }
            CommandResult::Ok
        }

        CommandAction::CloseChangeRequest { id } => {
            debug!(%id, "closing change request");
            if let Some(cr) = registry.change_requests.preferred() {
                let _ = cr.close_change_request(&repo.root, &id).await;
            }
            CommandResult::Ok
        }

        CommandAction::OpenIssue { id } => {
            debug!(%id, "opening issue in browser");
            if let Some(it) = registry.issue_trackers.preferred() {
                let _ = it.open_in_browser(&repo.root, &id).await;
            }
            CommandResult::Ok
        }

        CommandAction::LinkIssuesToChangeRequest { change_request_id, issue_ids } => {
            info!(issue_ids = ?issue_ids, %change_request_id, "linking issues to change request");
            let body_result = run!(runner, "gh", &["pr", "view", &change_request_id, "--json", "body", "--jq", ".body",], &repo.root,);
            match body_result {
                Ok(current_body) => {
                    let fixes_lines: Vec<String> = issue_ids.iter().map(|id| format!("Fixes #{id}")).collect();
                    let new_body = if current_body.trim().is_empty() {
                        fixes_lines.join("\n")
                    } else {
                        format!("{}\n\n{}", current_body.trim(), fixes_lines.join("\n"))
                    };
                    let result = run!(runner, "gh", &["pr", "edit", &change_request_id, "--body", &new_body], &repo.root,);
                    match result {
                        Ok(_) => {
                            info!(%change_request_id, "linked issues to change request");
                            CommandResult::Ok
                        }
                        Err(e) => {
                            error!(err = %e, "failed to edit change request");
                            CommandResult::Error { message: e }
                        }
                    }
                }
                Err(e) => {
                    error!(err = %e, "failed to read change request body");
                    CommandResult::Error { message: e }
                }
            }
        }

        CommandAction::ArchiveSession { session_id } => archive_session_result(&session_id, registry, providers_data).await,

        CommandAction::GenerateBranchName { issue_keys } => generate_branch_name_result(&issue_keys, registry, providers_data).await,

        CommandAction::TeleportSession { session_id, branch, checkout_key } => {
            info!(%session_id, "teleporting to session");
            let teleport_cmd = match resolve_attach_command(&session_id, registry, providers_data).await {
                Ok(cmd) => cmd,
                Err(message) => return CommandResult::Error { message },
            };
            let workspace_orchestrator =
                WorkspaceOrchestrator::new(&repo.root, registry, config_base, attachable_store, daemon_socket_path, local_host);
            let wt_path = if let Some(ref key) = checkout_key {
                let host_key = flotilla_protocol::HostPath::new(local_host.clone(), key.clone());
                providers_data.checkouts.get(&host_key).map(|_| key.clone())
            } else if let Some(branch_name) = &branch {
                let checkout_result = if let Some(cm) = registry.checkout_managers.preferred() {
                    cm.create_checkout(&repo.root, branch_name, false).await.ok()
                } else {
                    None
                };
                checkout_result.map(|(path, _)| path)
            } else {
                None
            };
            if let Some(path) = wt_path {
                let name = branch.as_deref().unwrap_or("session");
                if let Err(message) = workspace_orchestrator.create_workspace_for_teleport(&path, name, &teleport_cmd).await {
                    return CommandResult::Error { message };
                }
                CommandResult::Ok
            } else {
                CommandResult::Error { message: "Could not determine checkout path for teleport".to_string() }
            }
        }

        // These are handled at the daemon level (InProcessDaemon / SocketDaemon),
        // not by the per-repo executor. If they reach here, it's a routing bug.
        CommandAction::TrackRepoPath { .. }
        | CommandAction::UntrackRepo { .. }
        | CommandAction::Refresh { .. }
        | CommandAction::SetIssueViewport { .. }
        | CommandAction::FetchMoreIssues { .. }
        | CommandAction::SearchIssues { .. }
        | CommandAction::ClearIssueSearch { .. } => {
            CommandResult::Error { message: "bug: daemon-level command reached per-repo executor".to_string() }
        }
    }
}

async fn archive_session_result(session_id: &str, registry: &ProviderRegistry, providers_data: &ProviderData) -> CommandResult {
    if let Some(session) = providers_data.sessions.get(session_id) {
        info!(%session_id, "archiving session");
        if let Some(key) = session_provider_key(session, session_id) {
            if let Some((_, ca)) = registry.cloud_agents.get(key) {
                match ca.archive_session(session_id).await {
                    Ok(()) => CommandResult::Ok,
                    Err(e) => CommandResult::Error { message: e },
                }
            } else {
                CommandResult::Error { message: format!("No coding agent provider: {key}") }
            }
        } else {
            CommandResult::Error { message: format!("Cannot determine provider for session {session_id}") }
        }
    } else {
        CommandResult::Error { message: format!("session not found: {session_id}") }
    }
}

async fn generate_branch_name_result(issue_keys: &[String], registry: &ProviderRegistry, providers_data: &ProviderData) -> CommandResult {
    let issues: Vec<(String, String)> =
        issue_keys.iter().filter_map(|k| providers_data.issues.get(k.as_str()).map(|issue| (k.clone(), issue.title.clone()))).collect();

    let issue_id_pairs: Vec<(String, String)> = {
        let provider = registry.issue_trackers.preferred_name().map(|s| s.to_string()).unwrap_or_else(|| "issues".to_string());
        issues.iter().map(|(id, _title)| (provider.clone(), id.clone())).collect()
    };

    info!(issue_count = issue_keys.len(), "generating branch name");
    let branch_result = if let Some(ai) = registry.ai_utilities.preferred() {
        let context: Vec<String> = issues.iter().map(|(id, title)| format!("{} #{}", title, id)).collect();
        let prompt_text = if context.len() == 1 { context[0].clone() } else { context.join("; ") };
        Some(ai.generate_branch_name(&prompt_text).await)
    } else {
        None
    };

    match branch_result {
        Some(Ok(name)) => {
            info!(%name, "AI suggested");
            CommandResult::BranchNameGenerated { name, issue_ids: issue_id_pairs }
        }
        Some(Err(error)) => {
            warn!(%error, "using fallback branch name after AI failure");
            let fallback: Vec<String> = issues.iter().map(|(id, _)| format!("issue-{}", id)).collect();
            let name = fallback.join("-");
            CommandResult::BranchNameGenerated { name, issue_ids: issue_id_pairs }
        }
        None => {
            warn!("using fallback branch name without AI provider");
            let fallback: Vec<String> = issues.iter().map(|(id, _)| format!("issue-{}", id)).collect();
            let name = fallback.join("-");
            CommandResult::BranchNameGenerated { name, issue_ids: issue_id_pairs }
        }
    }
}

fn session_provider_key<'a>(session: &'a CloudAgentSession, session_id: &str) -> Option<&'a str> {
    session.correlation_keys.iter().find_map(|k| match k {
        CorrelationKey::SessionRef(provider, id) if id == session_id => Some(provider.as_str()),
        _ => None,
    })
}

async fn resolve_attach_command(session_id: &str, registry: &ProviderRegistry, providers_data: &ProviderData) -> Result<String, String> {
    let provider_key = providers_data
        .sessions
        .get(session_id)
        .and_then(|s| session_provider_key(s, session_id))
        .ok_or_else(|| format!("Cannot determine provider for session {session_id}"))?;

    let (_, ca) = registry.cloud_agents.get(provider_key).ok_or_else(|| format!("No coding agent provider: {provider_key}"))?;

    ca.attach_command(session_id).await
}
/// Build a WorkspaceConfig from repo/branch/dir/command.
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
        working_directory: working_dir.to_path_buf(),
        template_vars,
        template_yaml,
        resolved_commands: None,
    }
}

fn local_workspace_directory(repo_root: &Path, config_base: &Path) -> PathBuf {
    if repo_root.exists() {
        return repo_root.to_path_buf();
    }
    if let Some(home) = dirs::home_dir() {
        return home;
    }
    if let Ok(cwd) = std::env::current_dir() {
        return cwd;
    }
    config_base.to_path_buf()
}

#[cfg(test)]
mod tests;
