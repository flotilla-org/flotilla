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

use flotilla_protocol::{CheckoutSelector, CheckoutTarget, Command, CommandAction, CommandValue, HostName, HostPath};
use tracing::{debug, error, info};

use self::{
    checkout::{resolve_checkout_branch, write_branch_issue_links, CheckoutIntent, CheckoutService},
    session_actions::{resolve_attach_command, ReadOnlySessionActionService, TeleportFlow, TeleportSessionActionService},
    terminals::TerminalPreparationService,
    workspace::WorkspaceOrchestrator,
};
use crate::{
    attachable::SharedAttachableStore,
    data,
    provider_data::ProviderData,
    providers::{registry::ProviderRegistry, run, types::WorkspaceConfig, CommandRunner},
    step::{Step, StepAction, StepHost, StepOutcome, StepPlan, StepResolver},
};

/// The result of `build_plan`: either an immediate result or a multi-step plan.
pub enum ExecutionPlan {
    /// Command completed synchronously — no steps needed.
    Immediate(CommandValue),
    /// Command requires multiple steps with cancellation support.
    Steps(StepPlan),
}

#[derive(Clone)]
pub struct RepoExecutionContext {
    pub identity: flotilla_protocol::RepoIdentity,
    pub root: PathBuf,
}

enum CheckoutExistingPolicy {
    ReuseKnownCheckout,
    AlwaysCreate,
}

enum CheckoutIssueLinkPolicy {
    Inline,
    Deferred,
}

struct CheckoutFlow<'a> {
    branch: &'a str,
    create_branch: bool,
    intent: CheckoutIntent,
    issue_ids: &'a [(String, String)],
    repo_root: &'a Path,
    registry: &'a ProviderRegistry,
    providers_data: &'a ProviderData,
    runner: &'a dyn CommandRunner,
    local_host: &'a HostName,
}

impl<'a> CheckoutFlow<'a> {
    fn existing_checkout_path(&self) -> Option<PathBuf> {
        self.providers_data.checkouts.iter().find_map(|(hp, co)| {
            if hp.host == *self.local_host && co.branch == self.branch {
                Some(hp.path.clone())
            } else {
                None
            }
        })
    }

    async fn checkout_created_result(
        &self,
        existing_policy: CheckoutExistingPolicy,
        issue_link_policy: CheckoutIssueLinkPolicy,
    ) -> Result<CommandValue, String> {
        let checkout_service = CheckoutService::new(self.registry, self.runner);
        checkout_service.validate_target(self.repo_root, self.branch, self.intent).await?;

        if matches!(existing_policy, CheckoutExistingPolicy::ReuseKnownCheckout) {
            if let Some(path) = self.existing_checkout_path() {
                if matches!(self.intent, CheckoutIntent::FreshBranch) {
                    return Err(format!("branch already exists: {}", self.branch));
                }
                return Ok(CommandValue::CheckoutCreated { branch: self.branch.to_string(), path });
            }
        }

        let path = checkout_service.create_checkout(self.repo_root, self.branch, self.create_branch).await?;
        if !self.issue_ids.is_empty() && matches!(issue_link_policy, CheckoutIssueLinkPolicy::Inline) {
            checkout_service.write_branch_issue_links(self.repo_root, self.branch, self.issue_ids).await;
        }
        Ok(CommandValue::CheckoutCreated { branch: self.branch.to_string(), path })
    }
}

struct RemoveCheckoutFlow<'a> {
    checkout: &'a CheckoutSelector,
    repo_root: &'a Path,
    registry: &'a ProviderRegistry,
    providers_data: &'a ProviderData,
    runner: &'a dyn CommandRunner,
    local_host: &'a HostName,
    attachable_store: &'a SharedAttachableStore,
}

impl<'a> RemoveCheckoutFlow<'a> {
    fn resolve_branch(&self) -> Result<String, String> {
        resolve_checkout_branch(self.checkout, self.providers_data, self.local_host)
    }

    fn deleted_checkout_paths(&self, branch: &str) -> Vec<HostPath> {
        self.providers_data
            .checkouts
            .iter()
            .filter(|(hp, co)| co.branch == branch && hp.host == *self.local_host)
            .map(|(hp, _)| hp.clone())
            .collect()
    }

    async fn remove_branch(&self, branch: &str) -> Result<(), String> {
        let checkout_service = CheckoutService::new(self.registry, self.runner);
        let deleted_paths = self.deleted_checkout_paths(branch);
        checkout_service.remove_checkout(self.repo_root, branch, &deleted_paths, self.attachable_store).await
    }

    async fn execute(&self) -> CommandValue {
        let branch = match self.resolve_branch() {
            Ok(branch) => branch,
            Err(message) => return CommandValue::Error { message },
        };
        info!(%branch, "removing checkout");
        match self.remove_branch(&branch).await {
            Ok(()) => CommandValue::CheckoutRemoved { branch },
            Err(message) => CommandValue::Error { message },
        }
    }
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
            build_create_checkout_plan(branch, create_branch, intent, issue_ids)
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
            let remove_flow = RemoveCheckoutFlow {
                checkout: &checkout,
                repo_root: &repo.root,
                registry: registry.as_ref(),
                providers_data: providers_data.as_ref(),
                runner: runner.as_ref(),
                local_host: &local_host,
                attachable_store: &attachable_store,
            };
            match remove_flow.resolve_branch() {
                Ok(branch) => {
                    let deleted_paths = remove_flow.deleted_checkout_paths(&branch);
                    build_remove_checkout_plan(branch, deleted_paths)
                }
                Err(message) => ExecutionPlan::Immediate(CommandValue::Error { message }),
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
/// All steps are symbolic — the `ExecutorStepResolver` provides infrastructure
/// (registry, providers_data, runner, local_host) at execution time.
fn build_create_checkout_plan(
    branch: String,
    create_branch: bool,
    intent: CheckoutIntent,
    issue_ids: Vec<(String, String)>,
) -> ExecutionPlan {
    let mut steps = Vec::new();

    steps.push(Step {
        description: format!("Create checkout for branch {branch}"),
        host: StepHost::Local,
        action: StepAction::CreateCheckout { branch: branch.clone(), create_branch, intent, issue_ids: issue_ids.clone() },
    });

    if !issue_ids.is_empty() {
        steps.push(Step {
            description: "Link issues to branch".to_string(),
            host: StepHost::Local,
            action: StepAction::LinkIssuesToBranch { branch: branch.clone(), issue_ids },
        });
    }

    steps.push(Step {
        description: "Create workspace".to_string(),
        host: StepHost::Local,
        action: StepAction::CreateWorkspaceForCheckout { label: branch },
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
    let teleport_flow = TeleportFlow::new(
        &repo_root,
        registry.as_ref(),
        providers_data.as_ref(),
        &config_base,
        &attachable_store,
        daemon_socket_path.as_deref(),
        &local_host,
        &session_id,
        branch.as_deref(),
        checkout_key.as_ref(),
    );
    let initial_path = match teleport_flow.initial_checkout_path().await {
        Ok(path) => path,
        Err(message) => return ExecutionPlan::Immediate(CommandValue::Error { message }),
    };

    let steps = vec![
        Step {
            description: format!("Resolve attach command for session {session_id}"),
            host: StepHost::Local,
            action: StepAction::ResolveAttachCommand { session_id: session_id.clone() },
        },
        Step {
            description: "Ensure checkout for teleport".to_string(),
            host: StepHost::Local,
            action: StepAction::EnsureCheckoutForTeleport { branch: branch.clone(), checkout_key, initial_path },
        },
        Step {
            description: "Create workspace with teleport command".to_string(),
            host: StepHost::Local,
            action: StepAction::CreateTeleportWorkspace { session_id, branch },
        },
    ];

    ExecutionPlan::Steps(StepPlan::new(steps))
}

/// Build a step plan for `RemoveCheckout`.
///
/// Steps:
/// 1. Remove the checkout via the checkout manager
/// 2. Clean up correlated terminal sessions (best-effort)
fn build_remove_checkout_plan(branch: String, deleted_checkout_paths: Vec<HostPath>) -> ExecutionPlan {
    ExecutionPlan::Steps(StepPlan::new(vec![Step {
        description: format!("Remove checkout for branch {branch}"),
        host: StepHost::Local,
        action: StepAction::RemoveCheckout { branch, deleted_checkout_paths },
    }]))
}

/// Resolves symbolic `StepAction` variants using executor infrastructure.
pub(crate) struct ExecutorStepResolver {
    pub repo: RepoExecutionContext,
    pub registry: Arc<ProviderRegistry>,
    pub providers_data: Arc<ProviderData>,
    pub runner: Arc<dyn CommandRunner>,
    pub config_base: PathBuf,
    pub attachable_store: SharedAttachableStore,
    pub daemon_socket_path: Option<PathBuf>,
    pub local_host: HostName,
}

#[async_trait::async_trait]
impl StepResolver for ExecutorStepResolver {
    async fn resolve(&self, _description: &str, action: StepAction, prior: &[StepOutcome]) -> Result<StepOutcome, String> {
        match action {
            StepAction::CreateCheckout { branch, create_branch, intent, issue_ids } => {
                let checkout_flow = CheckoutFlow {
                    branch: &branch,
                    create_branch,
                    intent,
                    issue_ids: &issue_ids,
                    repo_root: &self.repo.root,
                    registry: self.registry.as_ref(),
                    providers_data: self.providers_data.as_ref(),
                    runner: self.runner.as_ref(),
                    local_host: &self.local_host,
                };
                let result = checkout_flow
                    .checkout_created_result(CheckoutExistingPolicy::ReuseKnownCheckout, CheckoutIssueLinkPolicy::Deferred)
                    .await?;
                if let CommandValue::CheckoutCreated { path, .. } = &result {
                    info!(checkout_path = %path.display(), "created checkout");
                }
                Ok(StepOutcome::CompletedWith(result))
            }
            StepAction::LinkIssuesToBranch { branch, issue_ids } => {
                write_branch_issue_links(&self.repo.root, &branch, &issue_ids, &*self.runner).await;
                Ok(StepOutcome::Completed)
            }
            StepAction::RemoveCheckout { branch, deleted_checkout_paths } => {
                let checkout_service = CheckoutService::new(self.registry.as_ref(), self.runner.as_ref());
                checkout_service.remove_checkout(&self.repo.root, &branch, &deleted_checkout_paths, &self.attachable_store).await?;
                Ok(StepOutcome::CompletedWith(CommandValue::CheckoutRemoved { branch }))
            }
            StepAction::ResolveAttachCommand { session_id } => {
                let cmd = resolve_attach_command(&session_id, self.registry.as_ref(), self.providers_data.as_ref()).await?;
                Ok(StepOutcome::Produced(CommandValue::AttachCommandResolved { command: cmd }))
            }
            StepAction::EnsureCheckoutForTeleport { branch, checkout_key, initial_path } => {
                if let Some(path) = initial_path {
                    return Ok(StepOutcome::Produced(CommandValue::CheckoutPathResolved { path }));
                }
                let service = TeleportSessionActionService::new(
                    &self.repo.root,
                    self.registry.as_ref(),
                    self.providers_data.as_ref(),
                    &self.config_base,
                    &self.attachable_store,
                    self.daemon_socket_path.as_deref(),
                    &self.local_host,
                );
                match service.resolve_teleport_checkout_path(checkout_key.as_ref(), branch.as_deref()).await? {
                    Some(path) => Ok(StepOutcome::Produced(CommandValue::CheckoutPathResolved { path })),
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

                let service = TeleportSessionActionService::new(
                    &self.repo.root,
                    self.registry.as_ref(),
                    self.providers_data.as_ref(),
                    &self.config_base,
                    &self.attachable_store,
                    self.daemon_socket_path.as_deref(),
                    &self.local_host,
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
            StepAction::CreateWorkspaceForCheckout { label } => {
                let path = prior.iter().find_map(|o| match o {
                    StepOutcome::CompletedWith(CommandValue::CheckoutCreated { path, .. }) => Some(path.clone()),
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
            #[cfg(test)]
            StepAction::Noop => Ok(StepOutcome::Completed),
        }
    }
}

async fn build_archive_session_plan(
    session_id: String,
    registry: Arc<ProviderRegistry>,
    providers_data: Arc<ProviderData>,
) -> ExecutionPlan {
    let session_actions = ReadOnlySessionActionService::new(registry.as_ref(), providers_data.as_ref());

    if !session_actions.should_run_archive_as_step(&session_id) {
        return ExecutionPlan::Immediate(session_actions.archive_session_result(&session_id).await);
    }

    ExecutionPlan::Steps(StepPlan::new(vec![Step {
        description: format!("Archive session {session_id}"),
        host: StepHost::Local,
        action: StepAction::ArchiveSession { session_id },
    }]))
}

async fn build_generate_branch_name_plan(
    issue_keys: Vec<String>,
    registry: Arc<ProviderRegistry>,
    providers_data: Arc<ProviderData>,
) -> ExecutionPlan {
    let session_actions = ReadOnlySessionActionService::new(registry.as_ref(), providers_data.as_ref());

    if !session_actions.should_run_generate_branch_name_as_step() {
        return ExecutionPlan::Immediate(session_actions.generate_branch_name_result(&issue_keys).await);
    }

    ExecutionPlan::Steps(StepPlan::new(vec![Step {
        description: "Generate branch name".to_string(),
        host: StepHost::Local,
        action: StepAction::GenerateBranchName { issue_keys },
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
) -> CommandValue {
    match action {
        CommandAction::CreateWorkspaceForCheckout { checkout_path, label } => {
            let host_key = HostPath::new(local_host.clone(), checkout_path.clone());
            if !providers_data.checkouts.contains_key(&host_key) {
                return CommandValue::Error { message: format!("checkout not found: {}", checkout_path.display()) };
            }
            info!(%label, "entering workspace");
            let workspace_orchestrator =
                WorkspaceOrchestrator::new(&repo.root, registry, config_base, attachable_store, daemon_socket_path, local_host);
            match workspace_orchestrator.create_workspace_for_checkout(&checkout_path, &label).await {
                Ok(_) => CommandValue::Ok,
                Err(e) => CommandValue::Error { message: e },
            }
        }

        CommandAction::CreateWorkspaceFromPreparedTerminal { target_host, branch, checkout_path, attachable_set_id, commands } => {
            let workspace_orchestrator =
                WorkspaceOrchestrator::new(&repo.root, registry, config_base, attachable_store, daemon_socket_path, local_host);
            if let Err(message) = workspace_orchestrator
                .create_workspace_from_prepared_terminal(&target_host, &branch, &checkout_path, attachable_set_id.as_ref(), &commands)
                .await
            {
                return CommandValue::Error { message };
            }
            CommandValue::Ok
        }

        CommandAction::SelectWorkspace { ws_ref } => {
            info!(%ws_ref, "switching to workspace");
            let workspace_orchestrator =
                WorkspaceOrchestrator::new(&repo.root, registry, config_base, attachable_store, daemon_socket_path, local_host);
            if let Err(message) = workspace_orchestrator.select_workspace(&ws_ref).await {
                return CommandValue::Error { message };
            }
            CommandValue::Ok
        }

        CommandAction::Checkout { target, issue_ids, .. } => {
            let (branch, create_branch, intent) = match target {
                CheckoutTarget::Branch(branch) => (branch, false, CheckoutIntent::ExistingBranch),
                CheckoutTarget::FreshBranch(branch) => (branch, true, CheckoutIntent::FreshBranch),
            };
            let checkout_flow = CheckoutFlow {
                branch: &branch,
                create_branch,
                intent,
                issue_ids: &issue_ids,
                repo_root: &repo.root,
                registry,
                providers_data,
                runner,
                local_host,
            };
            info!(%branch, "creating checkout");
            match checkout_flow.checkout_created_result(CheckoutExistingPolicy::AlwaysCreate, CheckoutIssueLinkPolicy::Inline).await {
                Ok(result) => {
                    if let CommandValue::CheckoutCreated { path, .. } = &result {
                        info!(checkout_path = %path.display(), "created checkout");
                    }
                    result
                }
                Err(message) => {
                    error!(err = %message, "create checkout failed");
                    CommandValue::Error { message }
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
                    Ok(commands) => CommandValue::TerminalPrepared {
                        repo_identity: repo.identity.clone(),
                        target_host: local_host.clone(),
                        branch: co.branch,
                        checkout_path,
                        attachable_set_id,
                        commands,
                    },
                    Err(message) => CommandValue::Error { message },
                }
            } else {
                CommandValue::Error { message: format!("checkout not found: {}", checkout_path.display()) }
            }
        }

        CommandAction::RemoveCheckout { checkout } => {
            RemoveCheckoutFlow {
                checkout: &checkout,
                repo_root: &repo.root,
                registry,
                providers_data,
                runner,
                local_host,
                attachable_store,
            }
            .execute()
            .await
        }

        CommandAction::FetchCheckoutStatus { branch, checkout_path, change_request_id } => {
            let info =
                data::fetch_checkout_status(&branch, checkout_path.as_deref(), change_request_id.as_deref(), &repo.root, runner).await;
            CommandValue::CheckoutStatus(info)
        }

        CommandAction::OpenChangeRequest { id } => {
            debug!(%id, "opening change request in browser");
            if let Some(cr) = registry.change_requests.preferred() {
                let _ = cr.open_in_browser(&repo.root, &id).await;
            }
            CommandValue::Ok
        }

        CommandAction::CloseChangeRequest { id } => {
            debug!(%id, "closing change request");
            if let Some(cr) = registry.change_requests.preferred() {
                let _ = cr.close_change_request(&repo.root, &id).await;
            }
            CommandValue::Ok
        }

        CommandAction::OpenIssue { id } => {
            debug!(%id, "opening issue in browser");
            if let Some(it) = registry.issue_trackers.preferred() {
                let _ = it.open_in_browser(&repo.root, &id).await;
            }
            CommandValue::Ok
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
                            CommandValue::Ok
                        }
                        Err(e) => {
                            error!(err = %e, "failed to edit change request");
                            CommandValue::Error { message: e }
                        }
                    }
                }
                Err(e) => {
                    error!(err = %e, "failed to read change request body");
                    CommandValue::Error { message: e }
                }
            }
        }

        CommandAction::ArchiveSession { session_id } => {
            let session_actions = ReadOnlySessionActionService::new(registry, providers_data);
            session_actions.archive_session_result(&session_id).await
        }

        CommandAction::GenerateBranchName { issue_keys } => {
            let session_actions = ReadOnlySessionActionService::new(registry, providers_data);
            session_actions.generate_branch_name_result(&issue_keys).await
        }

        CommandAction::TeleportSession { session_id, branch, checkout_key } => {
            info!(%session_id, "teleporting to session");
            let teleport_flow = TeleportFlow::new(
                &repo.root,
                registry,
                providers_data,
                config_base,
                attachable_store,
                daemon_socket_path,
                local_host,
                &session_id,
                branch.as_deref(),
                checkout_key.as_ref(),
            );
            match teleport_flow.execute().await {
                Ok(()) => CommandValue::Ok,
                Err(message) => CommandValue::Error { message },
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
            CommandValue::Error { message: "bug: daemon-level command reached per-repo executor".to_string() }
        }
    }
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
