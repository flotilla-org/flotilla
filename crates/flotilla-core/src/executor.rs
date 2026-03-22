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

use flotilla_protocol::{CheckoutTarget, Command, CommandAction, CommandValue, HostName, HostPath};
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

#[derive(Clone)]
pub struct RepoExecutionContext {
    pub identity: flotilla_protocol::RepoIdentity,
    pub root: PathBuf,
}

struct CheckoutFlow<'a> {
    branch: &'a str,
    create_branch: bool,
    intent: CheckoutIntent,
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

    async fn checkout_created_result(&self) -> Result<CommandValue, String> {
        let checkout_service = CheckoutService::new(self.registry, self.runner);
        checkout_service.validate_target(self.repo_root, self.branch, self.intent).await?;

        if let Some(path) = self.existing_checkout_path() {
            if matches!(self.intent, CheckoutIntent::FreshBranch) {
                return Err(format!("branch already exists: {}", self.branch));
            }
            return Ok(CommandValue::CheckoutCreated { branch: self.branch.to_string(), path });
        }

        let path = checkout_service.create_checkout(self.repo_root, self.branch, self.create_branch).await?;
        Ok(CommandValue::CheckoutCreated { branch: self.branch.to_string(), path })
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
    config_base: PathBuf,
    attachable_store: SharedAttachableStore,
    daemon_socket_path: Option<PathBuf>,
    local_host: HostName,
    // TODO(multi-host): When a command is forwarded from another host, this carries
    // the requester's hostname so plan builders can stamp `StepHost::Remote(originator)`
    // on steps that need to run back on the presentation host (e.g. workspace creation
    // after a remote checkout). Passed by `execute_forwarded_command` in server.rs.
    _originating_host: Option<HostName>,
) -> Result<StepPlan, CommandValue> {
    let Command { action, .. } = cmd;

    match action {
        CommandAction::Checkout { target, issue_ids, .. } => {
            let (branch, create_branch, intent) = match target {
                CheckoutTarget::Branch(branch) => (branch, false, CheckoutIntent::ExistingBranch),
                CheckoutTarget::FreshBranch(branch) => (branch, true, CheckoutIntent::FreshBranch),
            };
            Ok(build_create_checkout_plan(branch, create_branch, intent, issue_ids))
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
            match resolve_checkout_branch(&checkout, &providers_data, &local_host) {
                Ok(branch) => {
                    let deleted_paths: Vec<HostPath> = providers_data
                        .checkouts
                        .iter()
                        .filter(|(hp, co)| co.branch == branch && hp.host == local_host)
                        .map(|(hp, _)| hp.clone())
                        .collect();
                    Ok(build_remove_checkout_plan(branch, deleted_paths))
                }
                Err(message) => Err(CommandValue::Error { message }),
            }
        }

        CommandAction::ArchiveSession { session_id } => Ok(build_archive_session_plan(session_id)),

        CommandAction::GenerateBranchName { issue_keys } => Ok(build_generate_branch_name_plan(issue_keys)),

        CommandAction::CreateWorkspaceForCheckout { checkout_path, label } => Ok(StepPlan::new(vec![Step {
            description: format!("Create workspace for {label}"),
            host: StepHost::Local,
            action: StepAction::CreateWorkspaceForCheckout { label, checkout_path: Some(checkout_path) },
        }])),

        CommandAction::CreateWorkspaceFromPreparedTerminal { target_host, branch, checkout_path, attachable_set_id, commands } => {
            Ok(StepPlan::new(vec![Step {
                description: format!("Create workspace from prepared terminal for {branch}"),
                host: StepHost::Local,
                action: StepAction::CreateWorkspaceFromPreparedTerminal { target_host, branch, checkout_path, attachable_set_id, commands },
            }]))
        }

        CommandAction::SelectWorkspace { ws_ref } => Ok(StepPlan::new(vec![Step {
            description: format!("Select workspace {ws_ref}"),
            host: StepHost::Local,
            action: StepAction::SelectWorkspace { ws_ref },
        }])),

        CommandAction::PrepareTerminalForCheckout { checkout_path, commands } => Ok(StepPlan::new(vec![Step {
            description: "Prepare terminal for checkout".to_string(),
            host: StepHost::Local,
            action: StepAction::PrepareTerminalForCheckout { checkout_path, commands },
        }])),

        CommandAction::FetchCheckoutStatus { branch, checkout_path, change_request_id } => Ok(StepPlan::new(vec![Step {
            description: format!("Fetch checkout status for {branch}"),
            host: StepHost::Local,
            action: StepAction::FetchCheckoutStatus { branch, checkout_path, change_request_id },
        }])),

        CommandAction::OpenChangeRequest { id } => Ok(StepPlan::new(vec![Step {
            description: format!("Open change request {id}"),
            host: StepHost::Local,
            action: StepAction::OpenChangeRequest { id },
        }])),

        CommandAction::CloseChangeRequest { id } => Ok(StepPlan::new(vec![Step {
            description: format!("Close change request {id}"),
            host: StepHost::Local,
            action: StepAction::CloseChangeRequest { id },
        }])),

        CommandAction::OpenIssue { id } => Ok(StepPlan::new(vec![Step {
            description: format!("Open issue {id}"),
            host: StepHost::Local,
            action: StepAction::OpenIssue { id },
        }])),

        CommandAction::LinkIssuesToChangeRequest { change_request_id, issue_ids } => Ok(StepPlan::new(vec![Step {
            description: format!("Link issues to change request {change_request_id}"),
            host: StepHost::Local,
            action: StepAction::LinkIssuesToChangeRequest { change_request_id, issue_ids },
        }])),

        // Daemon-level commands should not reach build_plan.
        CommandAction::TrackRepoPath { .. }
        | CommandAction::UntrackRepo { .. }
        | CommandAction::Refresh { .. }
        | CommandAction::SetIssueViewport { .. }
        | CommandAction::FetchMoreIssues { .. }
        | CommandAction::SearchIssues { .. }
        | CommandAction::ClearIssueSearch { .. } => {
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
fn build_create_checkout_plan(branch: String, create_branch: bool, intent: CheckoutIntent, issue_ids: Vec<(String, String)>) -> StepPlan {
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
        action: StepAction::CreateWorkspaceForCheckout { label: branch, checkout_path: None },
    });

    StepPlan::new(steps)
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
) -> Result<StepPlan, CommandValue> {
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
        Err(message) => return Err(CommandValue::Error { message }),
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

    Ok(StepPlan::new(steps))
}

/// Build a step plan for `RemoveCheckout`.
///
/// Steps:
/// 1. Remove the checkout via the checkout manager
/// 2. Clean up correlated terminal sessions (best-effort)
fn build_remove_checkout_plan(branch: String, deleted_checkout_paths: Vec<HostPath>) -> StepPlan {
    StepPlan::new(vec![Step {
        description: format!("Remove checkout for branch {branch}"),
        host: StepHost::Local,
        action: StepAction::RemoveCheckout { branch, deleted_checkout_paths },
    }])
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
            StepAction::CreateCheckout { branch, create_branch, intent, .. } => {
                let checkout_flow = CheckoutFlow {
                    branch: &branch,
                    create_branch,
                    intent,
                    repo_root: &self.repo.root,
                    registry: self.registry.as_ref(),
                    providers_data: self.providers_data.as_ref(),
                    runner: self.runner.as_ref(),
                    local_host: &self.local_host,
                };
                let result = checkout_flow.checkout_created_result().await?;
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
            StepAction::CreateWorkspaceForCheckout { label, checkout_path: explicit_path } => {
                let path = if let Some(p) = explicit_path {
                    let host_key = HostPath::new(self.local_host.clone(), p.clone());
                    if !self.providers_data.checkouts.contains_key(&host_key) {
                        return Err(format!("checkout not found: {}", p.display()));
                    }
                    info!(%label, "entering workspace");
                    Some(p)
                } else {
                    prior.iter().find_map(|o| match o {
                        StepOutcome::CompletedWith(CommandValue::CheckoutCreated { path, .. }) => Some(path.clone()),
                        _ => None,
                    })
                };
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
            StepAction::CreateWorkspaceFromPreparedTerminal { target_host, branch, checkout_path, attachable_set_id, commands } => {
                let workspace_orchestrator = WorkspaceOrchestrator::new(
                    &self.repo.root,
                    self.registry.as_ref(),
                    &self.config_base,
                    &self.attachable_store,
                    self.daemon_socket_path.as_deref(),
                    &self.local_host,
                );
                workspace_orchestrator
                    .create_workspace_from_prepared_terminal(&target_host, &branch, &checkout_path, attachable_set_id.as_ref(), &commands)
                    .await?;
                Ok(StepOutcome::Completed)
            }
            StepAction::SelectWorkspace { ws_ref } => {
                info!(%ws_ref, "switching to workspace");
                let workspace_orchestrator = WorkspaceOrchestrator::new(
                    &self.repo.root,
                    self.registry.as_ref(),
                    &self.config_base,
                    &self.attachable_store,
                    self.daemon_socket_path.as_deref(),
                    &self.local_host,
                );
                workspace_orchestrator.select_workspace(&ws_ref).await?;
                Ok(StepOutcome::Completed)
            }
            StepAction::PrepareTerminalForCheckout { checkout_path, commands: requested_commands } => {
                let host_key = HostPath::new(self.local_host.clone(), checkout_path.clone());
                if let Some(co) = self.providers_data.checkouts.get(&host_key).cloned() {
                    let workspace_orchestrator = WorkspaceOrchestrator::new(
                        &self.repo.root,
                        self.registry.as_ref(),
                        &self.config_base,
                        &self.attachable_store,
                        self.daemon_socket_path.as_deref(),
                        &self.local_host,
                    );
                    let attachable_set_id = workspace_orchestrator.ensure_attachable_set_for_checkout(&self.local_host, &checkout_path);
                    let terminal_preparation = TerminalPreparationService::new(
                        self.registry.as_ref(),
                        &self.config_base,
                        &self.attachable_store,
                        self.daemon_socket_path.as_deref(),
                    );
                    let commands = terminal_preparation
                        .prepare_terminal_commands(&co.branch, &checkout_path, &requested_commands, || {
                            workspace_config(&self.repo.root, &co.branch, &checkout_path, "claude", &self.config_base)
                        })
                        .await?;
                    Ok(StepOutcome::CompletedWith(CommandValue::TerminalPrepared {
                        repo_identity: self.repo.identity.clone(),
                        target_host: self.local_host.clone(),
                        branch: co.branch,
                        checkout_path,
                        attachable_set_id,
                        commands,
                    }))
                } else {
                    Err(format!("checkout not found: {}", checkout_path.display()))
                }
            }
            StepAction::FetchCheckoutStatus { branch, checkout_path, change_request_id } => {
                let info = data::fetch_checkout_status(
                    &branch,
                    checkout_path.as_deref(),
                    change_request_id.as_deref(),
                    &self.repo.root,
                    self.runner.as_ref(),
                )
                .await;
                Ok(StepOutcome::CompletedWith(CommandValue::CheckoutStatus(info)))
            }
            StepAction::OpenChangeRequest { id } => {
                debug!(%id, "opening change request in browser");
                if let Some(cr) = self.registry.change_requests.preferred() {
                    let _ = cr.open_in_browser(&self.repo.root, &id).await;
                }
                Ok(StepOutcome::Completed)
            }
            StepAction::CloseChangeRequest { id } => {
                debug!(%id, "closing change request");
                if let Some(cr) = self.registry.change_requests.preferred() {
                    let _ = cr.close_change_request(&self.repo.root, &id).await;
                }
                Ok(StepOutcome::Completed)
            }
            StepAction::OpenIssue { id } => {
                debug!(%id, "opening issue in browser");
                if let Some(it) = self.registry.issue_trackers.preferred() {
                    let _ = it.open_in_browser(&self.repo.root, &id).await;
                }
                Ok(StepOutcome::Completed)
            }
            StepAction::LinkIssuesToChangeRequest { change_request_id, issue_ids } => {
                info!(issue_ids = ?issue_ids, %change_request_id, "linking issues to change request");
                let body_result = run!(
                    self.runner.as_ref(),
                    "gh",
                    &["pr", "view", &change_request_id, "--json", "body", "--jq", ".body"],
                    &self.repo.root
                );
                match body_result {
                    Ok(current_body) => {
                        let fixes_lines: Vec<String> = issue_ids.iter().map(|id| format!("Fixes #{id}")).collect();
                        let new_body = if current_body.trim().is_empty() {
                            fixes_lines.join("\n")
                        } else {
                            format!("{}\n\n{}", current_body.trim(), fixes_lines.join("\n"))
                        };
                        let result =
                            run!(self.runner.as_ref(), "gh", &["pr", "edit", &change_request_id, "--body", &new_body], &self.repo.root);
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
            #[cfg(test)]
            StepAction::Noop => Ok(StepOutcome::Completed),
        }
    }
}

fn build_archive_session_plan(session_id: String) -> StepPlan {
    StepPlan::new(vec![Step {
        description: format!("Archive session {session_id}"),
        host: StepHost::Local,
        action: StepAction::ArchiveSession { session_id },
    }])
}

fn build_generate_branch_name_plan(issue_keys: Vec<String>) -> StepPlan {
    StepPlan::new(vec![Step {
        description: "Generate branch name".to_string(),
        host: StepHost::Local,
        action: StepAction::GenerateBranchName { issue_keys },
    }])
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
