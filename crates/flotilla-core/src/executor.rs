//! Daemon-side command executor.
//!
//! Takes a `Command`, the repo context, and returns a `CommandResult`.
//! No UI state mutation — all results are carried in the return value.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use flotilla_protocol::{Command, CommandResult, ManagedTerminalId};
use tracing::{debug, error, info, warn};

use crate::{
    data,
    provider_data::ProviderData,
    providers::{
        registry::ProviderRegistry,
        run,
        terminal::TerminalPool,
        types::{CloudAgentSession, CorrelationKey, WorkspaceConfig},
        CommandRunner,
    },
    step::{Step, StepOutcome, StepPlan},
    template::{
        WorkspaceTemplate, {self},
    },
};

/// The result of `build_plan`: either an immediate result or a multi-step plan.
pub enum ExecutionPlan {
    /// Command completed synchronously — no steps needed.
    Immediate(CommandResult),
    /// Command requires multiple steps with cancellation support.
    Steps(StepPlan),
}

/// Build an execution plan for a command.
///
/// Multi-step commands (CreateCheckout, TeleportSession, RemoveCheckout) return
/// `ExecutionPlan::Steps` with cancellation points between steps. All other
/// commands delegate to `execute()` and return `ExecutionPlan::Immediate`.
pub async fn build_plan(
    cmd: Command,
    repo_root: PathBuf,
    registry: Arc<ProviderRegistry>,
    providers_data: Arc<ProviderData>,
    runner: Arc<dyn CommandRunner>,
    config_base: PathBuf,
) -> ExecutionPlan {
    let local_host = flotilla_protocol::HostName::local();

    match cmd {
        Command::CreateCheckout { branch, create_branch, issue_ids } => {
            build_create_checkout_plan(
                branch,
                create_branch,
                issue_ids,
                repo_root,
                registry,
                providers_data,
                runner,
                config_base,
                local_host,
            )
            .await
        }

        Command::TeleportSession { session_id, branch, checkout_key } => {
            build_teleport_session_plan(session_id, branch, checkout_key, repo_root, registry, providers_data, config_base, local_host)
                .await
        }

        Command::RemoveCheckout { branch, terminal_keys } => build_remove_checkout_plan(branch, terminal_keys, repo_root, registry),

        cmd => {
            let result = execute(cmd, &repo_root, &registry, &providers_data, &*runner, &config_base).await;
            ExecutionPlan::Immediate(result)
        }
    }
}

/// Build a step plan for `CreateCheckout`.
///
/// Steps:
/// 1. Create the checkout (skipped if it already exists on the local host)
/// 2. Link issues to the branch (skipped if no issue_ids)
/// 3. Create workspace (reads checkout path from shared slot)
#[allow(clippy::too_many_arguments)]
async fn build_create_checkout_plan(
    branch: String,
    create_branch: bool,
    issue_ids: Vec<(String, String)>,
    repo_root: PathBuf,
    registry: Arc<ProviderRegistry>,
    providers_data: Arc<ProviderData>,
    runner: Arc<dyn CommandRunner>,
    config_base: PathBuf,
    local_host: flotilla_protocol::HostName,
) -> ExecutionPlan {
    // Shared slot for the checkout path — pre-populated if the checkout already exists.
    let checkout_path_slot: Arc<tokio::sync::Mutex<Option<PathBuf>>> = {
        let existing = providers_data.checkouts.iter().find_map(|(hp, co)| {
            if hp.host == local_host && co.branch == branch {
                Some(hp.path.clone())
            } else {
                None
            }
        });
        Arc::new(tokio::sync::Mutex::new(existing))
    };

    let mut steps = Vec::new();

    // Step 1: Create checkout
    {
        let slot = Arc::clone(&checkout_path_slot);
        let branch = branch.clone();
        let repo_root = repo_root.clone();
        let registry = Arc::clone(&registry);
        steps.push(Step {
            description: format!("Create checkout for branch {branch}"),
            action: Box::new(move || {
                Box::pin(async move {
                    // Skip if checkout already exists
                    if slot.lock().await.is_some() {
                        return Ok(StepOutcome::Skipped);
                    }
                    let cm = registry
                        .checkout_managers
                        .values()
                        .next()
                        .map(|(_, cm)| Arc::clone(cm))
                        .ok_or_else(|| "No checkout manager available".to_string())?;
                    let (path, _checkout) = cm.create_checkout(&repo_root, &branch, create_branch).await?;
                    info!(checkout_path = %path.display(), "created checkout");
                    *slot.lock().await = Some(path);
                    Ok(StepOutcome::CompletedWith(CommandResult::CheckoutCreated { branch }))
                })
            }),
        });
    }

    // Step 2: Link issues (only if non-empty)
    if !issue_ids.is_empty() {
        let branch = branch.clone();
        let repo_root = repo_root.clone();
        let runner = Arc::clone(&runner);
        steps.push(Step {
            description: "Link issues to branch".to_string(),
            action: Box::new(move || {
                Box::pin(async move {
                    write_branch_issue_links(&repo_root, &branch, &issue_ids, &*runner).await;
                    Ok(StepOutcome::Completed)
                })
            }),
        });
    }

    // Step 3: Create workspace
    {
        let slot = Arc::clone(&checkout_path_slot);
        let branch = branch.clone();
        let repo_root = repo_root.clone();
        let registry = Arc::clone(&registry);
        let config_base = config_base.clone();
        steps.push(Step {
            description: "Create workspace".to_string(),
            action: Box::new(move || {
                Box::pin(async move {
                    let checkout_path = slot.lock().await.clone().ok_or_else(|| "checkout path not available".to_string())?;
                    if let Some((_, ws_mgr)) = &registry.workspace_manager {
                        let mut config = workspace_config(&repo_root, &branch, &checkout_path, "claude", &config_base);
                        if let Some((_, tp)) = &registry.terminal_pool {
                            resolve_terminal_pool(&mut config, tp.as_ref()).await;
                        }
                        if let Err(e) = ws_mgr.create_workspace(&config).await {
                            // Checkout was created but workspace failed — log but don't fail
                            error!(err = %e, "workspace creation failed after checkout");
                        }
                    }
                    Ok(StepOutcome::Completed)
                })
            }),
        });
    }

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
            action: Box::new(move || {
                Box::pin(async move {
                    let cmd = resolve_attach_command(&session_id, &registry, &providers_data).await?;
                    *slot.lock().await = Some(cmd);
                    Ok(StepOutcome::Completed)
                })
            }),
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
            action: Box::new(move || {
                Box::pin(async move {
                    // Already have a checkout path — skip
                    if slot.lock().await.is_some() {
                        return Ok(StepOutcome::Skipped);
                    }
                    let branch_name = match &branch {
                        Some(b) => b.clone(),
                        None => return Ok(StepOutcome::Skipped),
                    };
                    let cm = registry
                        .checkout_managers
                        .values()
                        .next()
                        .map(|(_, cm)| Arc::clone(cm))
                        .ok_or_else(|| "No checkout manager available".to_string())?;
                    let (path, _checkout) = cm.create_checkout(&repo_root, &branch_name, false).await?;
                    *slot.lock().await = Some(path);
                    Ok(StepOutcome::Completed)
                })
            }),
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
        steps.push(Step {
            description: "Create workspace with teleport command".to_string(),
            action: Box::new(move || {
                Box::pin(async move {
                    let path =
                        path_slot.lock().await.clone().ok_or_else(|| "Could not determine checkout path for teleport".to_string())?;
                    let teleport_cmd = teleport_slot.lock().await.clone().ok_or_else(|| "Attach command not resolved".to_string())?;
                    let name = branch.as_deref().unwrap_or("session");
                    if let Some((_, ws_mgr)) = &registry.workspace_manager {
                        let mut config = workspace_config(&repo_root, name, &path, &teleport_cmd, &config_base);
                        if let Some((_, tp)) = &registry.terminal_pool {
                            resolve_terminal_pool(&mut config, tp.as_ref()).await;
                        }
                        // Unlike CreateCheckout, teleport fails entirely if the workspace
                        // can't be created — the checkout may already have existed.
                        ws_mgr.create_workspace(&config).await?;
                    }
                    Ok(StepOutcome::Completed)
                })
            }),
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
) -> ExecutionPlan {
    let mut steps = Vec::new();

    // Step 1: Remove checkout
    {
        let branch = branch.clone();
        let repo_root = repo_root.clone();
        let registry = Arc::clone(&registry);
        steps.push(Step {
            description: format!("Remove checkout for branch {branch}"),
            action: Box::new(move || {
                Box::pin(async move {
                    let cm = registry
                        .checkout_managers
                        .values()
                        .next()
                        .map(|(_, cm)| Arc::clone(cm))
                        .ok_or_else(|| "No checkout manager available".to_string())?;
                    cm.remove_checkout(&repo_root, &branch).await?;
                    Ok(StepOutcome::Completed)
                })
            }),
        });
    }

    // Step 2: Clean up terminal sessions (best-effort)
    if !terminal_keys.is_empty() {
        let registry = Arc::clone(&registry);
        steps.push(Step {
            description: "Clean up terminal sessions".to_string(),
            action: Box::new(move || {
                Box::pin(async move {
                    if let Some((_, tp)) = &registry.terminal_pool {
                        for terminal_id in &terminal_keys {
                            if let Err(e) = tp.kill_terminal(terminal_id).await {
                                warn!(
                                    terminal = %terminal_id,
                                    err = %e,
                                    "failed to kill terminal session (best-effort)"
                                );
                            }
                        }
                    }
                    Ok(StepOutcome::Completed)
                })
            }),
        });
    }

    ExecutionPlan::Steps(StepPlan::new(steps))
}

/// Execute a `Command` against the given repo context.
///
/// Commands that are handled at the daemon level (AddRepo, RemoveRepo, Refresh)
/// should not reach this function — the caller should handle them directly.
pub async fn execute(
    cmd: Command,
    repo_root: &Path,
    registry: &ProviderRegistry,
    providers_data: &ProviderData,
    runner: &dyn CommandRunner,
    config_base: &Path,
) -> CommandResult {
    let local_host = flotilla_protocol::HostName::local();
    match cmd {
        Command::CreateWorkspaceForCheckout { checkout_path } => {
            let host_key = flotilla_protocol::HostPath::new(local_host.clone(), checkout_path.clone());
            if let Some(co) = providers_data.checkouts.get(&host_key).cloned() {
                info!(branch = %co.branch, "entering workspace");
                if let Some((_, ws_mgr)) = &registry.workspace_manager {
                    let mut config = workspace_config(repo_root, &co.branch, &checkout_path, "claude", config_base);
                    if let Some((_, tp)) = &registry.terminal_pool {
                        resolve_terminal_pool(&mut config, tp.as_ref()).await;
                    }
                    if let Err(e) = ws_mgr.create_workspace(&config).await {
                        return CommandResult::Error { message: e };
                    }
                }
                CommandResult::Ok
            } else {
                CommandResult::Error { message: format!("checkout not found: {}", checkout_path.display()) }
            }
        }

        Command::SelectWorkspace { ws_ref } => {
            info!(%ws_ref, "switching to workspace");
            if let Some((_, ws_mgr)) = &registry.workspace_manager {
                if let Err(e) = ws_mgr.select_workspace(&ws_ref).await {
                    return CommandResult::Error { message: e };
                }
            }
            CommandResult::Ok
        }

        Command::CreateCheckout { branch, create_branch, issue_ids } => {
            info!(%branch, "creating checkout");
            let checkout_result = if let Some((_, cm)) = registry.checkout_managers.values().next() {
                Some(cm.create_checkout(repo_root, &branch, create_branch).await)
            } else {
                None
            };
            match checkout_result {
                Some(Ok((checkout_path, _checkout))) => {
                    // Write issue links to git config
                    if !issue_ids.is_empty() {
                        write_branch_issue_links(repo_root, &branch, &issue_ids, runner).await;
                    }
                    info!(checkout_path = %checkout_path.display(), "created checkout");
                    // Create workspace if manager available
                    if let Some((_, ws_mgr)) = &registry.workspace_manager {
                        let mut config = workspace_config(repo_root, &branch, &checkout_path, "claude", config_base);
                        if let Some((_, tp)) = &registry.terminal_pool {
                            resolve_terminal_pool(&mut config, tp.as_ref()).await;
                        }
                        if let Err(e) = ws_mgr.create_workspace(&config).await {
                            // Checkout was created but workspace failed — report as error
                            // but the checkout still exists
                            error!(err = %e, "workspace creation failed after checkout");
                        }
                    }
                    CommandResult::CheckoutCreated { branch: branch.clone() }
                }
                Some(Err(e)) => {
                    error!(err = %e, "create checkout failed");
                    CommandResult::Error { message: e }
                }
                None => CommandResult::Error { message: "No checkout manager available".to_string() },
            }
        }

        Command::RemoveCheckout { branch, terminal_keys } => {
            info!(%branch, "removing checkout");
            let result = if let Some((_, cm)) = registry.checkout_managers.values().next() {
                Some(cm.remove_checkout(repo_root, &branch).await)
            } else {
                None
            };
            match result {
                Some(Ok(())) => {
                    // Best-effort cleanup of correlated terminal sessions
                    if let Some((_, tp)) = &registry.terminal_pool {
                        for terminal_id in &terminal_keys {
                            if let Err(e) = tp.kill_terminal(terminal_id).await {
                                warn!(
                                    terminal = %terminal_id,
                                    err = %e,
                                    "failed to kill terminal session (best-effort)"
                                );
                            }
                        }
                    }
                    CommandResult::Ok
                }
                Some(Err(e)) => CommandResult::Error { message: e },
                None => CommandResult::Error { message: "No checkout manager available".to_string() },
            }
        }

        Command::FetchCheckoutStatus { branch, checkout_path, change_request_id } => {
            let info =
                data::fetch_checkout_status(&branch, checkout_path.as_deref(), change_request_id.as_deref(), repo_root, runner).await;
            CommandResult::CheckoutStatus(info)
        }

        Command::OpenChangeRequest { id } => {
            debug!(%id, "opening change request in browser");
            if let Some((_, cr)) = registry.code_review.values().next() {
                let _ = cr.open_in_browser(repo_root, &id).await;
            }
            CommandResult::Ok
        }

        Command::CloseChangeRequest { id } => {
            debug!(%id, "closing change request");
            if let Some((_, cr)) = registry.code_review.values().next() {
                let _ = cr.close_change_request(repo_root, &id).await;
            }
            CommandResult::Ok
        }

        Command::OpenIssue { id } => {
            debug!(%id, "opening issue in browser");
            if let Some((_, it)) = registry.issue_trackers.values().next() {
                let _ = it.open_in_browser(repo_root, &id).await;
            }
            CommandResult::Ok
        }

        Command::LinkIssuesToChangeRequest { change_request_id, issue_ids } => {
            info!(issue_ids = ?issue_ids, %change_request_id, "linking issues to change request");
            let body_result = run!(runner, "gh", &["pr", "view", &change_request_id, "--json", "body", "--jq", ".body",], repo_root,);
            match body_result {
                Ok(current_body) => {
                    let fixes_lines: Vec<String> = issue_ids.iter().map(|id| format!("Fixes #{id}")).collect();
                    let new_body = if current_body.trim().is_empty() {
                        fixes_lines.join("\n")
                    } else {
                        format!("{}\n\n{}", current_body.trim(), fixes_lines.join("\n"))
                    };
                    let result = run!(runner, "gh", &["pr", "edit", &change_request_id, "--body", &new_body], repo_root,);
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

        Command::ArchiveSession { session_id } => {
            if let Some(session) = providers_data.sessions.get(session_id.as_str()) {
                info!(%session_id, "archiving session");
                if let Some(key) = session_provider_key(session, &session_id) {
                    if let Some((_, ca)) = registry.cloud_agents.get(key) {
                        match ca.archive_session(&session_id).await {
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

        Command::GenerateBranchName { issue_keys } => {
            let issues: Vec<(String, String)> = issue_keys
                .iter()
                .filter_map(|k| providers_data.issues.get(k.as_str()).map(|issue| (k.clone(), issue.title.clone())))
                .collect();

            // Collect (provider_name, issue_id) pairs for the created branch
            let issue_id_pairs: Vec<(String, String)> = {
                let provider = registry.issue_trackers.keys().next().cloned().unwrap_or_else(|| "issues".to_string());
                issues.iter().map(|(id, _title)| (provider.clone(), id.clone())).collect()
            };

            info!("generating branch name");
            let branch_result = if let Some((_, ai)) = registry.ai_utilities.values().next() {
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
                _ => {
                    let fallback: Vec<String> = issues.iter().map(|(id, _)| format!("issue-{}", id)).collect();
                    let name = fallback.join("-");
                    CommandResult::BranchNameGenerated { name, issue_ids: issue_id_pairs }
                }
            }
        }

        Command::TeleportSession { session_id, branch, checkout_key } => {
            info!(%session_id, "teleporting to session");
            let teleport_cmd = match resolve_attach_command(&session_id, registry, providers_data).await {
                Ok(cmd) => cmd,
                Err(message) => return CommandResult::Error { message },
            };
            let wt_path = if let Some(ref key) = checkout_key {
                let host_key = flotilla_protocol::HostPath::new(local_host.clone(), key.clone());
                providers_data.checkouts.get(&host_key).map(|_| key.clone())
            } else if let Some(branch_name) = &branch {
                let checkout_result = if let Some((_, cm)) = registry.checkout_managers.values().next() {
                    cm.create_checkout(repo_root, branch_name, false).await.ok()
                } else {
                    None
                };
                checkout_result.map(|(path, _)| path)
            } else {
                None
            };
            if let Some(path) = wt_path {
                let name = branch.as_deref().unwrap_or("session");
                if let Some((_, ws_mgr)) = &registry.workspace_manager {
                    let mut config = workspace_config(repo_root, name, &path, &teleport_cmd, config_base);
                    if let Some((_, tp)) = &registry.terminal_pool {
                        resolve_terminal_pool(&mut config, tp.as_ref()).await;
                    }
                    if let Err(e) = ws_mgr.create_workspace(&config).await {
                        // Unlike CreateCheckout, teleport fails entirely if the workspace
                        // can't be created — the checkout may already have existed.
                        return CommandResult::Error { message: e };
                    }
                }
                CommandResult::Ok
            } else {
                CommandResult::Error { message: "Could not determine checkout path for teleport".to_string() }
            }
        }

        // These are handled at the daemon level (InProcessDaemon / SocketDaemon),
        // not by the per-repo executor. If they reach here, it's a routing bug.
        Command::AddRepo { .. }
        | Command::RemoveRepo { .. }
        | Command::Refresh
        | Command::SetIssueViewport { .. }
        | Command::FetchMoreIssues { .. }
        | Command::SearchIssues { .. }
        | Command::ClearIssueSearch { .. } => {
            CommandResult::Error { message: "bug: daemon-level command reached per-repo executor".to_string() }
        }
    }
}

/// Resolve terminal sessions through the pool. Each terminal content entry is
/// ensured running and its attach command is stored in `config.resolved_commands`.
async fn resolve_terminal_pool(config: &mut WorkspaceConfig, terminal_pool: &dyn TerminalPool) {
    let tmpl = if let Some(ref yaml) = config.template_yaml {
        serde_yml::from_str::<WorkspaceTemplate>(yaml).unwrap_or_else(|e| {
            warn!(err = %e, "failed to parse workspace template, using default");
            template::default_template()
        })
    } else {
        template::default_template()
    };
    let rendered = tmpl.render(&config.template_vars);
    info!(count = rendered.content.len(), "terminal pool: resolving content entries",);
    let mut resolved = Vec::new();
    for entry in &rendered.content {
        if entry.content_type != "terminal" {
            debug!(
                role = %entry.role,
                content_type = %entry.content_type,
                "skipping non-terminal content",
            );
            continue;
        }
        let count = entry.count.unwrap_or(1);
        for i in 0..count {
            let id = ManagedTerminalId { checkout: config.name.clone(), role: entry.role.clone(), index: i };
            if let Err(e) = terminal_pool.ensure_running(&id, &entry.command, &config.working_directory).await {
                warn!(%id, err = %e, "failed to ensure terminal");
                continue;
            }
            match terminal_pool.attach_command(&id, &entry.command, &config.working_directory).await {
                Ok(cmd) => {
                    debug!(%id, command = ?entry.command, resolved = ?cmd, "terminal resolved");
                    resolved.push((entry.role.clone(), cmd));
                }
                Err(e) => warn!(%id, err = %e, "failed to get attach command"),
            }
        }
    }
    info!(count = resolved.len(), "terminal pool: resolved commands");
    if !resolved.is_empty() {
        config.resolved_commands = Some(resolved);
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

/// Write branch-to-issue links into git config.
async fn write_branch_issue_links(repo_root: &Path, branch: &str, issue_ids: &[(String, String)], runner: &dyn CommandRunner) {
    use std::collections::HashMap;
    let mut by_provider: HashMap<&str, Vec<&str>> = HashMap::new();
    for (provider, id) in issue_ids {
        by_provider.entry(provider.as_str()).or_default().push(id.as_str());
    }
    for (provider, ids) in by_provider {
        let key = format!("branch.{branch}.flotilla.issues.{provider}");
        let value = ids.join(",");
        if let Err(e) = run!(runner, "git", &["config", &key, &value], repo_root) {
            tracing::warn!(err = %e, "failed to write issue link");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{path::PathBuf, sync::Arc};

    use super::*;
    use crate::providers::{
        ai_utility::AiUtility, code_review::CodeReview, coding_agent::CloudAgentService, discovery::ProviderDescriptor,
        issue_tracker::IssueTracker, testing::MockRunner, types::*, vcs::CheckoutManager, workspace::WorkspaceManager,
    };

    fn desc(name: &str) -> ProviderDescriptor {
        ProviderDescriptor::named(name)
    }
    use async_trait::async_trait;
    use flotilla_protocol::{HostName, HostPath};

    fn hp(path: &str) -> HostPath {
        HostPath::new(HostName::local(), PathBuf::from(path))
    }

    // -----------------------------------------------------------------------
    // Mock providers
    // -----------------------------------------------------------------------

    /// A mock CheckoutManager that returns a canned checkout or error.
    struct MockCheckoutManager {
        create_result: tokio::sync::Mutex<Option<Result<(PathBuf, Checkout), String>>>,
        remove_result: tokio::sync::Mutex<Option<Result<(), String>>>,
    }

    impl MockCheckoutManager {
        fn succeeding(branch: &str, path: &str) -> Self {
            Self {
                create_result: tokio::sync::Mutex::new(Some(Ok((PathBuf::from(path), Checkout {
                    branch: branch.to_string(),
                    is_main: false,
                    trunk_ahead_behind: None,
                    remote_ahead_behind: None,
                    working_tree: None,
                    last_commit: None,
                    correlation_keys: vec![],
                    association_keys: vec![],
                })))),
                remove_result: tokio::sync::Mutex::new(Some(Ok(()))),
            }
        }

        fn failing(msg: &str) -> Self {
            Self {
                create_result: tokio::sync::Mutex::new(Some(Err(msg.to_string()))),
                remove_result: tokio::sync::Mutex::new(Some(Err(msg.to_string()))),
            }
        }
    }

    #[async_trait]
    impl CheckoutManager for MockCheckoutManager {
        async fn list_checkouts(&self, _repo_root: &Path) -> Result<Vec<(PathBuf, Checkout)>, String> {
            Ok(vec![])
        }
        async fn create_checkout(&self, _repo_root: &Path, _branch: &str, _create_branch: bool) -> Result<(PathBuf, Checkout), String> {
            self.create_result.lock().await.take().expect("create_checkout called more than expected")
        }
        async fn remove_checkout(&self, _repo_root: &Path, _branch: &str) -> Result<(), String> {
            self.remove_result.lock().await.take().expect("remove_checkout called more than expected")
        }
    }

    /// A mock WorkspaceManager that records calls and returns configurable results.
    struct MockWorkspaceManager {
        create_result: tokio::sync::Mutex<Result<(), String>>,
        select_result: tokio::sync::Mutex<Result<(), String>>,
    }

    impl MockWorkspaceManager {
        fn succeeding() -> Self {
            Self { create_result: tokio::sync::Mutex::new(Ok(())), select_result: tokio::sync::Mutex::new(Ok(())) }
        }

        fn failing(msg: &str) -> Self {
            Self {
                create_result: tokio::sync::Mutex::new(Err(msg.to_string())),
                select_result: tokio::sync::Mutex::new(Err(msg.to_string())),
            }
        }
    }

    #[async_trait]
    impl WorkspaceManager for MockWorkspaceManager {
        async fn list_workspaces(&self) -> Result<Vec<(String, Workspace)>, String> {
            Ok(vec![])
        }
        async fn create_workspace(&self, _config: &WorkspaceConfig) -> Result<(String, Workspace), String> {
            let result = self.create_result.lock().await;
            match &*result {
                Ok(()) => {
                    Ok(("mock-ref".to_string(), Workspace { name: "mock".to_string(), directories: vec![], correlation_keys: vec![] }))
                }
                Err(e) => Err(e.clone()),
            }
        }
        async fn select_workspace(&self, _ws_ref: &str) -> Result<(), String> {
            let result = self.select_result.lock().await;
            result.clone()
        }
    }

    /// A mock CodeReview provider.
    struct MockCodeReview;

    #[async_trait]
    impl CodeReview for MockCodeReview {
        async fn list_change_requests(&self, _repo_root: &Path, _limit: usize) -> Result<Vec<(String, ChangeRequest)>, String> {
            Ok(vec![])
        }
        async fn get_change_request(&self, _repo_root: &Path, _id: &str) -> Result<(String, ChangeRequest), String> {
            Err("not implemented".to_string())
        }
        async fn open_in_browser(&self, _repo_root: &Path, _id: &str) -> Result<(), String> {
            Ok(())
        }
        async fn close_change_request(&self, _repo_root: &Path, _id: &str) -> Result<(), String> {
            Ok(())
        }
        async fn list_merged_branch_names(&self, _repo_root: &Path, _limit: usize) -> Result<Vec<String>, String> {
            Ok(vec![])
        }
    }

    /// A mock IssueTracker provider.
    struct MockIssueTracker;

    #[async_trait]
    impl IssueTracker for MockIssueTracker {
        async fn list_issues(&self, _repo_root: &Path, _limit: usize) -> Result<Vec<(String, Issue)>, String> {
            Ok(vec![])
        }
        async fn open_in_browser(&self, _repo_root: &Path, _id: &str) -> Result<(), String> {
            Ok(())
        }
    }

    /// A mock CloudAgentService provider.
    struct MockCloudAgent {
        archive_result: tokio::sync::Mutex<Result<(), String>>,
        attach_command: String,
    }

    impl MockCloudAgent {
        fn succeeding() -> Self {
            Self { archive_result: tokio::sync::Mutex::new(Ok(())), attach_command: "mock-attach-cmd".to_string() }
        }

        fn failing(msg: &str) -> Self {
            Self { archive_result: tokio::sync::Mutex::new(Err(msg.to_string())), attach_command: "mock-attach-cmd".to_string() }
        }

        fn with_attach(attach_command: &str) -> Self {
            Self { archive_result: tokio::sync::Mutex::new(Ok(())), attach_command: attach_command.to_string() }
        }
    }

    #[async_trait]
    impl CloudAgentService for MockCloudAgent {
        async fn list_sessions(&self, _criteria: &RepoCriteria) -> Result<Vec<(String, CloudAgentSession)>, String> {
            Ok(vec![])
        }
        async fn archive_session(&self, _session_id: &str) -> Result<(), String> {
            let result = self.archive_result.lock().await;
            result.clone()
        }
        async fn attach_command(&self, session_id: &str) -> Result<String, String> {
            Ok(format!("{} {session_id}", self.attach_command))
        }
    }

    /// A mock AiUtility provider.
    struct MockAiUtility {
        result: tokio::sync::Mutex<Result<String, String>>,
    }

    impl MockAiUtility {
        fn succeeding(name: &str) -> Self {
            Self { result: tokio::sync::Mutex::new(Ok(name.to_string())) }
        }

        fn failing(msg: &str) -> Self {
            Self { result: tokio::sync::Mutex::new(Err(msg.to_string())) }
        }
    }

    #[async_trait]
    impl AiUtility for MockAiUtility {
        async fn generate_branch_name(&self, _context: &str) -> Result<String, String> {
            let result = self.result.lock().await;
            result.clone()
        }
    }

    // -----------------------------------------------------------------------
    // Helper to build test fixtures
    // -----------------------------------------------------------------------

    fn empty_registry() -> ProviderRegistry {
        ProviderRegistry::new()
    }

    fn empty_data() -> ProviderData {
        ProviderData::default()
    }

    fn repo_root() -> PathBuf {
        PathBuf::from("/tmp/test-repo")
    }

    fn config_base() -> PathBuf {
        PathBuf::from("/tmp/test-config")
    }

    fn make_checkout(branch: &str, _path: &str) -> Checkout {
        Checkout {
            branch: branch.to_string(),
            is_main: false,
            trunk_ahead_behind: None,
            remote_ahead_behind: None,
            working_tree: None,
            last_commit: None,
            correlation_keys: vec![],
            association_keys: vec![],
        }
    }

    fn make_session_for(provider: &str, id: &str) -> CloudAgentSession {
        CloudAgentSession {
            title: "test session".to_string(),
            status: SessionStatus::Running,
            model: None,
            updated_at: None,
            correlation_keys: vec![CorrelationKey::SessionRef(provider.to_string(), id.to_string())],
            provider_name: String::new(),
            provider_display_name: String::new(),
            item_noun: String::new(),
        }
    }

    fn make_issue(_id: &str, title: &str) -> Issue {
        Issue {
            title: title.to_string(),
            labels: vec![],
            association_keys: vec![],
            provider_name: String::new(),
            provider_display_name: String::new(),
        }
    }

    fn runner_ok() -> MockRunner {
        MockRunner::new(vec![])
    }

    async fn run_execute(
        command: Command,
        registry: &ProviderRegistry,
        providers_data: &ProviderData,
        runner: &MockRunner,
    ) -> CommandResult {
        execute(command, &repo_root(), registry, providers_data, runner, &config_base()).await
    }

    fn assert_error_contains(result: CommandResult, expected_substring: &str) {
        match result {
            CommandResult::Error { message } => {
                assert!(message.contains(expected_substring), "expected error containing {expected_substring:?}, got {message:?}");
            }
            other => panic!("expected Error, got {:?}", other),
        }
    }

    fn assert_error_eq(result: CommandResult, expected: &str) {
        match result {
            CommandResult::Error { message } => assert_eq!(message, expected),
            other => panic!("expected Error, got {:?}", other),
        }
    }

    fn assert_checkout_created_branch(result: CommandResult, expected_branch: &str) {
        match result {
            CommandResult::CheckoutCreated { branch } => {
                assert_eq!(branch, expected_branch);
            }
            other => panic!("expected CheckoutCreated, got {:?}", other),
        }
    }

    fn assert_checkout_status_branch(result: CommandResult, expected_branch: &str) {
        match result {
            CommandResult::CheckoutStatus(info) => {
                assert_eq!(info.branch, expected_branch);
            }
            other => panic!("expected CheckoutStatus, got {:?}", other),
        }
    }

    fn assert_branch_name_generated(result: CommandResult, expected_name: &str, expected_issue_ids: &[(&str, &str)]) {
        match result {
            CommandResult::BranchNameGenerated { name, issue_ids } => {
                assert_eq!(name, expected_name);
                let expected_issue_ids: Vec<_> =
                    expected_issue_ids.iter().map(|(provider, id)| (provider.to_string(), id.to_string())).collect();
                assert_eq!(issue_ids, expected_issue_ids);
            }
            other => panic!("expected BranchNameGenerated, got {:?}", other),
        }
    }

    fn assert_ok(result: CommandResult) {
        assert!(matches!(result, CommandResult::Ok));
    }

    // -----------------------------------------------------------------------
    // Tests: CreateWorkspaceForCheckout
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn create_workspace_for_checkout_success_without_ws_manager() {
        let registry = empty_registry();
        let mut data = empty_data();
        let path = PathBuf::from("/repo/wt-feat");
        data.checkouts.insert(hp("/repo/wt-feat"), make_checkout("feat", "/repo/wt-feat"));
        let runner = runner_ok();

        let result = run_execute(Command::CreateWorkspaceForCheckout { checkout_path: path }, &registry, &data, &runner).await;

        assert_ok(result);
    }

    #[tokio::test]
    async fn archive_session_uses_provider_from_session_ref() {
        let mut registry = empty_registry();
        registry.cloud_agents.insert("claude".to_string(), (desc("claude"), Arc::new(MockCloudAgent::failing("wrong provider"))));
        registry.cloud_agents.insert("cursor".to_string(), (desc("cursor"), Arc::new(MockCloudAgent::succeeding())));
        let mut data = empty_data();
        data.sessions.insert("sess-1".to_string(), make_session_for("cursor", "sess-1"));
        let runner = runner_ok();

        let result = run_execute(Command::ArchiveSession { session_id: "sess-1".to_string() }, &registry, &data, &runner).await;

        assert_ok(result);
    }

    #[tokio::test]
    async fn create_workspace_for_checkout_success_with_ws_manager() {
        let mut registry = empty_registry();
        registry.workspace_manager = Some((desc("cmux"), Arc::new(MockWorkspaceManager::succeeding())));
        let mut data = empty_data();
        let path = PathBuf::from("/repo/wt-feat");
        data.checkouts.insert(hp("/repo/wt-feat"), make_checkout("feat", "/repo/wt-feat"));
        let runner = runner_ok();

        let result = run_execute(Command::CreateWorkspaceForCheckout { checkout_path: path }, &registry, &data, &runner).await;

        assert_ok(result);
    }

    #[tokio::test]
    async fn create_workspace_for_checkout_not_found() {
        let registry = empty_registry();
        let data = empty_data();
        let runner = runner_ok();

        let result =
            run_execute(Command::CreateWorkspaceForCheckout { checkout_path: PathBuf::from("/nonexistent") }, &registry, &data, &runner)
                .await;

        assert_error_contains(result, "checkout not found");
    }

    #[tokio::test]
    async fn create_workspace_for_checkout_ws_manager_fails() {
        let mut registry = empty_registry();
        registry.workspace_manager = Some((desc("cmux"), Arc::new(MockWorkspaceManager::failing("ws creation failed"))));
        let mut data = empty_data();
        let path = PathBuf::from("/repo/wt-feat");
        data.checkouts.insert(hp("/repo/wt-feat"), make_checkout("feat", "/repo/wt-feat"));
        let runner = runner_ok();

        let result = run_execute(Command::CreateWorkspaceForCheckout { checkout_path: path }, &registry, &data, &runner).await;

        assert_error_eq(result, "ws creation failed");
    }

    // -----------------------------------------------------------------------
    // Tests: SelectWorkspace
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn select_workspace_no_manager() {
        let registry = empty_registry();
        let runner = runner_ok();

        let result = run_execute(Command::SelectWorkspace { ws_ref: "my-ws".to_string() }, &registry, &empty_data(), &runner).await;

        assert_ok(result);
    }

    #[tokio::test]
    async fn select_workspace_success() {
        let mut registry = empty_registry();
        registry.workspace_manager = Some((desc("cmux"), Arc::new(MockWorkspaceManager::succeeding())));
        let runner = runner_ok();

        let result = run_execute(Command::SelectWorkspace { ws_ref: "my-ws".to_string() }, &registry, &empty_data(), &runner).await;

        assert_ok(result);
    }

    #[tokio::test]
    async fn select_workspace_failure() {
        let mut registry = empty_registry();
        registry.workspace_manager = Some((desc("cmux"), Arc::new(MockWorkspaceManager::failing("select failed"))));
        let runner = runner_ok();

        let result = run_execute(Command::SelectWorkspace { ws_ref: "bad-ws".to_string() }, &registry, &empty_data(), &runner).await;

        assert_error_eq(result, "select failed");
    }

    // -----------------------------------------------------------------------
    // Tests: CreateCheckout
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn create_checkout_no_manager() {
        let registry = empty_registry();
        let runner = runner_ok();

        let result = run_execute(
            Command::CreateCheckout { branch: "feat-x".to_string(), create_branch: true, issue_ids: vec![] },
            &registry,
            &empty_data(),
            &runner,
        )
        .await;

        assert_error_contains(result, "No checkout manager available");
    }

    #[tokio::test]
    async fn create_checkout_success() {
        let mut registry = empty_registry();
        registry
            .checkout_managers
            .insert("wt".to_string(), (desc("wt"), Arc::new(MockCheckoutManager::succeeding("feat-x", "/repo/wt-feat-x"))));
        let runner = runner_ok();

        let result = run_execute(
            Command::CreateCheckout { branch: "feat-x".to_string(), create_branch: true, issue_ids: vec![] },
            &registry,
            &empty_data(),
            &runner,
        )
        .await;

        assert_checkout_created_branch(result, "feat-x");
    }

    #[tokio::test]
    async fn create_checkout_with_issue_ids_writes_git_config() {
        let mut registry = empty_registry();
        registry
            .checkout_managers
            .insert("wt".to_string(), (desc("wt"), Arc::new(MockCheckoutManager::succeeding("feat-x", "/repo/wt-feat-x"))));
        // The runner needs one Ok response for the git config write
        let runner = MockRunner::new(vec![Ok(String::new())]);

        let result = run_execute(
            Command::CreateCheckout {
                branch: "feat-x".to_string(),
                create_branch: true,
                issue_ids: vec![("github".to_string(), "42".to_string())],
            },
            &registry,
            &empty_data(),
            &runner,
        )
        .await;

        assert_checkout_created_branch(result, "feat-x");
    }

    #[tokio::test]
    async fn create_checkout_failure() {
        let mut registry = empty_registry();
        registry.checkout_managers.insert("wt".to_string(), (desc("wt"), Arc::new(MockCheckoutManager::failing("branch already exists"))));
        let runner = runner_ok();

        let result = run_execute(
            Command::CreateCheckout { branch: "feat-x".to_string(), create_branch: true, issue_ids: vec![] },
            &registry,
            &empty_data(),
            &runner,
        )
        .await;

        assert_error_eq(result, "branch already exists");
    }

    #[tokio::test]
    async fn create_checkout_success_ws_manager_fails_still_returns_created() {
        let mut registry = empty_registry();
        registry
            .checkout_managers
            .insert("wt".to_string(), (desc("wt"), Arc::new(MockCheckoutManager::succeeding("feat-x", "/repo/wt-feat-x"))));
        registry.workspace_manager = Some((desc("cmux"), Arc::new(MockWorkspaceManager::failing("ws failed"))));
        let runner = runner_ok();

        let result = run_execute(
            Command::CreateCheckout { branch: "feat-x".to_string(), create_branch: true, issue_ids: vec![] },
            &registry,
            &empty_data(),
            &runner,
        )
        .await;

        // Workspace failure is logged but checkout still reports success
        assert_checkout_created_branch(result, "feat-x");
    }

    // -----------------------------------------------------------------------
    // Tests: RemoveCheckout
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn remove_checkout_no_manager() {
        let registry = empty_registry();
        let runner = runner_ok();

        let result =
            run_execute(Command::RemoveCheckout { branch: "old".to_string(), terminal_keys: vec![] }, &registry, &empty_data(), &runner)
                .await;

        assert_error_contains(result, "No checkout manager available");
    }

    #[tokio::test]
    async fn remove_checkout_success() {
        let mut registry = empty_registry();
        registry.checkout_managers.insert("wt".to_string(), (desc("wt"), Arc::new(MockCheckoutManager::succeeding("old", "/repo/wt-old"))));
        let runner = runner_ok();

        let result =
            run_execute(Command::RemoveCheckout { branch: "old".to_string(), terminal_keys: vec![] }, &registry, &empty_data(), &runner)
                .await;

        assert_ok(result);
    }

    #[tokio::test]
    async fn remove_checkout_failure() {
        let mut registry = empty_registry();
        registry.checkout_managers.insert("wt".to_string(), (desc("wt"), Arc::new(MockCheckoutManager::failing("cannot remove trunk"))));
        let runner = runner_ok();

        let result =
            run_execute(Command::RemoveCheckout { branch: "main".to_string(), terminal_keys: vec![] }, &registry, &empty_data(), &runner)
                .await;

        assert_error_eq(result, "cannot remove trunk");
    }

    // -----------------------------------------------------------------------
    // Tests: RemoveCheckout — terminal cleanup
    // -----------------------------------------------------------------------

    struct MockTerminalPool {
        killed: tokio::sync::Mutex<Vec<ManagedTerminalId>>,
    }

    #[async_trait]
    impl TerminalPool for MockTerminalPool {
        async fn list_terminals(&self) -> Result<Vec<flotilla_protocol::ManagedTerminal>, String> {
            Ok(vec![])
        }
        async fn ensure_running(&self, _id: &ManagedTerminalId, _cmd: &str, _cwd: &Path) -> Result<(), String> {
            Ok(())
        }
        async fn attach_command(&self, _id: &ManagedTerminalId, _cmd: &str, _cwd: &Path) -> Result<String, String> {
            Ok(String::new())
        }
        async fn kill_terminal(&self, id: &ManagedTerminalId) -> Result<(), String> {
            self.killed.lock().await.push(id.clone());
            Ok(())
        }
    }

    #[tokio::test]
    async fn remove_checkout_kills_correlated_terminals() {
        let terminal_id = ManagedTerminalId { checkout: "feat-x".into(), role: "shell".into(), index: 0 };
        let mock_pool = Arc::new(MockTerminalPool { killed: tokio::sync::Mutex::new(vec![]) });

        let mut registry = empty_registry();
        registry
            .checkout_managers
            .insert("wt".to_string(), (desc("wt"), Arc::new(MockCheckoutManager::succeeding("feat-x", "/repo/wt-feat-x"))));
        registry.terminal_pool = Some((desc("shpool"), Arc::clone(&mock_pool) as Arc<dyn TerminalPool>));

        let runner = runner_ok();
        let result = run_execute(
            Command::RemoveCheckout { branch: "feat-x".into(), terminal_keys: vec![terminal_id.clone()] },
            &registry,
            &empty_data(),
            &runner,
        )
        .await;

        assert_ok(result);
        let killed = mock_pool.killed.lock().await;
        assert_eq!(killed.len(), 1);
        assert_eq!(killed[0], terminal_id);
    }

    // -----------------------------------------------------------------------
    // Tests: FetchCheckoutStatus
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn fetch_checkout_status_returns_checkout_status() {
        let registry = empty_registry();
        // fetch_checkout_status runs multiple git/gh commands concurrently via
        // tokio::join!. Provide enough error responses for all subprocess calls:
        //   - git rev-parse (upstream) -> Err
        //   - git rev-parse (origin/HEAD) -> Err
        //   - git status --porcelain -> Err
        //   - gh pr view -> Err
        let runner = MockRunner::new(vec![Err("err".to_string()), Err("err".to_string()), Err("err".to_string()), Err("err".to_string())]);

        let result = run_execute(
            Command::FetchCheckoutStatus {
                branch: "feat".to_string(),
                checkout_path: Some(PathBuf::from("/repo/wt")),
                change_request_id: Some("42".to_string()),
            },
            &registry,
            &empty_data(),
            &runner,
        )
        .await;

        assert_checkout_status_branch(result, "feat");
    }

    #[tokio::test]
    async fn fetch_checkout_status_populates_uncommitted_files() {
        let registry = empty_registry();
        let runner = MockRunner::new(vec![
            Err("err".to_string()),
            Err("err".to_string()),
            Ok(" M src/main.rs\n?? TODO.txt\n".to_string()),
            Err("err".to_string()),
        ]);

        let result = run_execute(
            Command::FetchCheckoutStatus {
                branch: "feat".to_string(),
                checkout_path: Some(PathBuf::from("/repo/wt")),
                change_request_id: None,
            },
            &registry,
            &empty_data(),
            &runner,
        )
        .await;

        match result {
            CommandResult::CheckoutStatus(info) => {
                assert!(info.has_uncommitted);
                assert_eq!(info.uncommitted_files, vec![" M src/main.rs".to_string(), "?? TODO.txt".to_string(),]);
            }
            other => panic!("expected CheckoutStatus, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // Tests: OpenChangeRequest
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn open_change_request_no_provider() {
        let registry = empty_registry();
        let runner = runner_ok();

        let result = run_execute(Command::OpenChangeRequest { id: "42".to_string() }, &registry, &empty_data(), &runner).await;

        assert_ok(result);
    }

    #[tokio::test]
    async fn open_change_request_with_provider() {
        let mut registry = empty_registry();
        registry.code_review.insert("github".to_string(), (desc("github"), Arc::new(MockCodeReview)));
        let runner = runner_ok();

        let result = run_execute(Command::OpenChangeRequest { id: "42".to_string() }, &registry, &empty_data(), &runner).await;

        assert_ok(result);
    }

    // -----------------------------------------------------------------------
    // Tests: CloseChangeRequest
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn close_change_request_no_provider() {
        let registry = empty_registry();
        let runner = runner_ok();

        let result = run_execute(Command::CloseChangeRequest { id: "42".to_string() }, &registry, &empty_data(), &runner).await;

        assert_ok(result);
    }

    #[tokio::test]
    async fn close_change_request_with_provider() {
        let mut registry = empty_registry();
        registry.code_review.insert("github".to_string(), (desc("github"), Arc::new(MockCodeReview)));
        let runner = runner_ok();

        let result = run_execute(Command::CloseChangeRequest { id: "42".to_string() }, &registry, &empty_data(), &runner).await;

        assert_ok(result);
    }

    // -----------------------------------------------------------------------
    // Tests: OpenIssue
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn open_issue_no_provider() {
        let registry = empty_registry();
        let runner = runner_ok();

        let result = run_execute(Command::OpenIssue { id: "10".to_string() }, &registry, &empty_data(), &runner).await;

        assert_ok(result);
    }

    #[tokio::test]
    async fn open_issue_with_provider() {
        let mut registry = empty_registry();
        registry.issue_trackers.insert("github".to_string(), (desc("github"), Arc::new(MockIssueTracker)));
        let runner = runner_ok();

        let result = run_execute(Command::OpenIssue { id: "10".to_string() }, &registry, &empty_data(), &runner).await;

        assert_ok(result);
    }

    // -----------------------------------------------------------------------
    // Tests: LinkIssuesToChangeRequest
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn link_issues_success_with_existing_body() {
        let registry = empty_registry();
        // First call: gh pr view returns existing body
        // Second call: gh pr edit succeeds
        let runner = MockRunner::new(vec![Ok("Existing PR body".to_string()), Ok(String::new())]);

        let result = run_execute(
            Command::LinkIssuesToChangeRequest { change_request_id: "55".to_string(), issue_ids: vec!["10".to_string(), "20".to_string()] },
            &registry,
            &empty_data(),
            &runner,
        )
        .await;

        assert_ok(result);
    }

    #[tokio::test]
    async fn link_issues_success_with_empty_body() {
        let registry = empty_registry();
        let runner = MockRunner::new(vec![
            Ok("  \n".to_string()), // empty/whitespace body
            Ok(String::new()),      // edit succeeds
        ]);

        let result = run_execute(
            Command::LinkIssuesToChangeRequest { change_request_id: "55".to_string(), issue_ids: vec!["10".to_string()] },
            &registry,
            &empty_data(),
            &runner,
        )
        .await;

        assert_ok(result);
    }

    #[tokio::test]
    async fn link_issues_view_fails() {
        let registry = empty_registry();
        let runner = MockRunner::new(vec![Err("gh not found".to_string())]);

        let result = run_execute(
            Command::LinkIssuesToChangeRequest { change_request_id: "55".to_string(), issue_ids: vec!["10".to_string()] },
            &registry,
            &empty_data(),
            &runner,
        )
        .await;

        assert_error_eq(result, "gh not found");
    }

    #[tokio::test]
    async fn link_issues_edit_fails() {
        let registry = empty_registry();
        let runner = MockRunner::new(vec![Ok("body text".to_string()), Err("permission denied".to_string())]);

        let result = run_execute(
            Command::LinkIssuesToChangeRequest { change_request_id: "55".to_string(), issue_ids: vec!["10".to_string()] },
            &registry,
            &empty_data(),
            &runner,
        )
        .await;

        assert_error_eq(result, "permission denied");
    }

    // -----------------------------------------------------------------------
    // Tests: ArchiveSession
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn archive_session_not_found() {
        let registry = empty_registry();
        let runner = runner_ok();

        let result =
            run_execute(Command::ArchiveSession { session_id: "nonexistent".to_string() }, &registry, &empty_data(), &runner).await;

        assert_error_contains(result, "session not found");
    }

    #[tokio::test]
    async fn archive_session_no_agent_provider() {
        let registry = empty_registry();
        let mut data = empty_data();
        data.sessions.insert("sess-1".to_string(), make_session_for("claude", "sess-1"));
        let runner = runner_ok();

        let result = run_execute(Command::ArchiveSession { session_id: "sess-1".to_string() }, &registry, &data, &runner).await;

        assert_error_contains(result, "No coding agent provider: claude");
    }

    #[tokio::test]
    async fn archive_session_success() {
        let mut registry = empty_registry();
        registry.cloud_agents.insert("claude".to_string(), (desc("claude"), Arc::new(MockCloudAgent::succeeding())));
        let mut data = empty_data();
        data.sessions.insert("sess-1".to_string(), make_session_for("claude", "sess-1"));
        let runner = runner_ok();

        let result = run_execute(Command::ArchiveSession { session_id: "sess-1".to_string() }, &registry, &data, &runner).await;

        assert_ok(result);
    }

    #[tokio::test]
    async fn archive_session_agent_fails() {
        let mut registry = empty_registry();
        registry.cloud_agents.insert("claude".to_string(), (desc("claude"), Arc::new(MockCloudAgent::failing("archive failed"))));
        let mut data = empty_data();
        data.sessions.insert("sess-1".to_string(), make_session_for("claude", "sess-1"));
        let runner = runner_ok();

        let result = run_execute(Command::ArchiveSession { session_id: "sess-1".to_string() }, &registry, &data, &runner).await;

        assert_error_eq(result, "archive failed");
    }

    // -----------------------------------------------------------------------
    // Tests: GenerateBranchName
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn generate_branch_name_ai_success() {
        let mut registry = empty_registry();
        registry.ai_utilities.insert("claude".to_string(), (desc("claude"), Arc::new(MockAiUtility::succeeding("feat/add-login"))));
        registry.issue_trackers.insert("github".to_string(), (desc("github"), Arc::new(MockIssueTracker)));
        let mut data = empty_data();
        data.issues.insert("42".to_string(), make_issue("42", "Add login feature"));
        let runner = runner_ok();

        let result = run_execute(Command::GenerateBranchName { issue_keys: vec!["42".to_string()] }, &registry, &data, &runner).await;

        assert_branch_name_generated(result, "feat/add-login", &[("github", "42")]);
    }

    #[tokio::test]
    async fn generate_branch_name_ai_failure_uses_fallback() {
        let mut registry = empty_registry();
        registry.ai_utilities.insert("claude".to_string(), (desc("claude"), Arc::new(MockAiUtility::failing("API error"))));
        let mut data = empty_data();
        data.issues.insert("42".to_string(), make_issue("42", "Add login"));
        let runner = runner_ok();

        let result = run_execute(Command::GenerateBranchName { issue_keys: vec!["42".to_string()] }, &registry, &data, &runner).await;

        assert_branch_name_generated(result, "issue-42", &[("issues", "42")]);
    }

    #[tokio::test]
    async fn generate_branch_name_no_ai_provider_uses_fallback() {
        let registry = empty_registry();
        let mut data = empty_data();
        data.issues.insert("7".to_string(), make_issue("7", "Fix bug"));
        let runner = runner_ok();

        let result = run_execute(Command::GenerateBranchName { issue_keys: vec!["7".to_string()] }, &registry, &data, &runner).await;

        // No issue tracker registered, defaults to "issues"
        assert_branch_name_generated(result, "issue-7", &[("issues", "7")]);
    }

    #[tokio::test]
    async fn generate_branch_name_multiple_issues() {
        let mut registry = empty_registry();
        registry.ai_utilities.insert("claude".to_string(), (desc("claude"), Arc::new(MockAiUtility::succeeding("feat/login-and-signup"))));
        registry.issue_trackers.insert("github".to_string(), (desc("github"), Arc::new(MockIssueTracker)));
        let mut data = empty_data();
        data.issues.insert("1".to_string(), make_issue("1", "Login feature"));
        data.issues.insert("2".to_string(), make_issue("2", "Signup feature"));
        let runner = runner_ok();

        let result =
            run_execute(Command::GenerateBranchName { issue_keys: vec!["1".to_string(), "2".to_string()] }, &registry, &data, &runner)
                .await;

        assert_branch_name_generated(result, "feat/login-and-signup", &[("github", "1"), ("github", "2")]);
    }

    #[tokio::test]
    async fn generate_branch_name_unknown_issue_key() {
        let registry = empty_registry();
        let data = empty_data();
        let runner = runner_ok();

        let result =
            run_execute(Command::GenerateBranchName { issue_keys: vec!["nonexistent".to_string()] }, &registry, &data, &runner).await;

        // No issues found, so empty fallback
        assert_branch_name_generated(result, "", &[]);
    }

    // -----------------------------------------------------------------------
    // Tests: TeleportSession
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn teleport_session_with_checkout_key() {
        let mut registry = empty_registry();
        registry.cloud_agents.insert(
            "claude".to_string(),
            (desc("claude"), Arc::new(MockCloudAgent::with_attach("claude --teleport"))), // base; mock appends session_id
        );
        registry.workspace_manager = Some((desc("cmux"), Arc::new(MockWorkspaceManager::succeeding())));
        let mut data = empty_data();
        let path = PathBuf::from("/repo/wt-feat");
        data.checkouts.insert(hp("/repo/wt-feat"), make_checkout("feat", "/repo/wt-feat"));
        data.sessions.insert("sess-1".to_string(), make_session_for("claude", "sess-1"));
        let runner = runner_ok();

        let result = run_execute(
            Command::TeleportSession { session_id: "sess-1".to_string(), branch: Some("feat".to_string()), checkout_key: Some(path) },
            &registry,
            &data,
            &runner,
        )
        .await;

        assert_ok(result);
    }

    #[tokio::test]
    async fn teleport_session_uses_provider_specific_attach_command() {
        let mut registry = empty_registry();
        registry.cloud_agents.insert("claude".to_string(), (desc("claude"), Arc::new(MockCloudAgent::with_attach("claude --teleport"))));
        registry.cloud_agents.insert("cursor".to_string(), (desc("cursor"), Arc::new(MockCloudAgent::with_attach("agent --resume"))));
        registry.workspace_manager = Some((desc("cmux"), Arc::new(MockWorkspaceManager::succeeding())));
        let mut data = empty_data();
        let path = PathBuf::from("/repo/wt-feat");
        data.checkouts.insert(hp("/repo/wt-feat"), make_checkout("feat", "/repo/wt-feat"));
        data.sessions.insert("sess-1".to_string(), make_session_for("cursor", "sess-1"));
        let runner = runner_ok();

        let attach = resolve_attach_command("sess-1", &registry, &data).await.expect("resolve attach command");
        assert_eq!(attach, "agent --resume sess-1");

        let result = run_execute(
            Command::TeleportSession { session_id: "sess-1".to_string(), branch: Some("feat".to_string()), checkout_key: Some(path) },
            &registry,
            &data,
            &runner,
        )
        .await;

        assert_ok(result);
    }

    #[tokio::test]
    async fn teleport_session_with_branch_creates_checkout() {
        let mut registry = empty_registry();
        registry.cloud_agents.insert("claude".to_string(), (desc("claude"), Arc::new(MockCloudAgent::succeeding())));
        registry
            .checkout_managers
            .insert("wt".to_string(), (desc("wt"), Arc::new(MockCheckoutManager::succeeding("feat", "/repo/wt-feat"))));
        registry.workspace_manager = Some((desc("cmux"), Arc::new(MockWorkspaceManager::succeeding())));
        let mut data = empty_data();
        data.sessions.insert("sess-1".to_string(), make_session_for("claude", "sess-1"));
        let runner = runner_ok();

        let result = run_execute(
            Command::TeleportSession { session_id: "sess-1".to_string(), branch: Some("feat".to_string()), checkout_key: None },
            &registry,
            &data,
            &runner,
        )
        .await;

        assert_ok(result);
    }

    #[tokio::test]
    async fn teleport_session_no_path_no_branch() {
        let mut registry = empty_registry();
        registry.cloud_agents.insert("claude".to_string(), (desc("claude"), Arc::new(MockCloudAgent::succeeding())));
        let mut data = empty_data();
        data.sessions.insert("sess-1".to_string(), make_session_for("claude", "sess-1"));
        let runner = runner_ok();

        let result = run_execute(
            Command::TeleportSession { session_id: "sess-1".to_string(), branch: None, checkout_key: None },
            &registry,
            &data,
            &runner,
        )
        .await;

        assert_error_contains(result, "Could not determine checkout path");
    }

    #[tokio::test]
    async fn teleport_session_ws_manager_fails() {
        let mut registry = empty_registry();
        registry.cloud_agents.insert("claude".to_string(), (desc("claude"), Arc::new(MockCloudAgent::succeeding())));
        registry.workspace_manager = Some((desc("cmux"), Arc::new(MockWorkspaceManager::failing("ws failed"))));
        let mut data = empty_data();
        let path = PathBuf::from("/repo/wt-feat");
        data.checkouts.insert(hp("/repo/wt-feat"), make_checkout("feat", "/repo/wt-feat"));
        data.sessions.insert("sess-1".to_string(), make_session_for("claude", "sess-1"));
        let runner = runner_ok();

        let result = run_execute(
            Command::TeleportSession { session_id: "sess-1".to_string(), branch: Some("feat".to_string()), checkout_key: Some(path) },
            &registry,
            &data,
            &runner,
        )
        .await;

        assert_error_eq(result, "ws failed");
    }

    #[tokio::test]
    async fn teleport_session_uses_session_as_name_when_no_branch() {
        // When checkout_key is present but branch is None, uses "session" as name.
        let mut registry = empty_registry();
        registry.cloud_agents.insert("claude".to_string(), (desc("claude"), Arc::new(MockCloudAgent::succeeding())));
        registry.workspace_manager = Some((desc("cmux"), Arc::new(MockWorkspaceManager::succeeding())));
        let mut data = empty_data();
        let path = PathBuf::from("/repo/wt-feat");
        data.checkouts.insert(hp("/repo/wt-feat"), make_checkout("feat", "/repo/wt-feat"));
        data.sessions.insert("sess-1".to_string(), make_session_for("claude", "sess-1"));
        let runner = runner_ok();

        let result = run_execute(
            Command::TeleportSession { session_id: "sess-1".to_string(), branch: None, checkout_key: Some(path) },
            &registry,
            &data,
            &runner,
        )
        .await;

        assert_ok(result);
    }

    // -----------------------------------------------------------------------
    // Tests: Daemon-level commands rejected
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn daemon_level_commands_return_error() {
        let registry = empty_registry();
        let data = empty_data();
        let runner = runner_ok();

        let daemon_commands = vec![
            Command::AddRepo { path: PathBuf::from("/repo") },
            Command::RemoveRepo { path: PathBuf::from("/repo") },
            Command::Refresh,
            Command::SetIssueViewport { repo: PathBuf::from("/repo"), visible_count: 10 },
            Command::FetchMoreIssues { repo: PathBuf::from("/repo"), desired_count: 20 },
            Command::SearchIssues { repo: PathBuf::from("/repo"), query: "bug".to_string() },
            Command::ClearIssueSearch { repo: PathBuf::from("/repo") },
        ];

        for cmd in daemon_commands {
            let result = run_execute(cmd, &registry, &data, &runner).await;
            assert_error_contains(result, "daemon-level command");
        }
    }

    // -----------------------------------------------------------------------
    // Tests: workspace_config helper
    // -----------------------------------------------------------------------

    #[test]
    fn workspace_config_builds_correct_struct() {
        let config = workspace_config(Path::new("/nonexistent-repo"), "my-branch", Path::new("/repo/wt"), "claude", &config_base());

        assert_eq!(config.name, "my-branch");
        assert_eq!(config.working_directory, PathBuf::from("/repo/wt"));
        assert_eq!(config.template_vars.get("main_command"), Some(&"claude".to_string()));
        assert!(config.template_yaml.is_none(), "no template file should exist at test paths");
    }

    // -----------------------------------------------------------------------
    // Helper to run build_plan with Arc-wrapped arguments
    // -----------------------------------------------------------------------

    async fn run_build_plan(
        command: Command,
        registry: ProviderRegistry,
        providers_data: ProviderData,
        runner: MockRunner,
    ) -> ExecutionPlan {
        build_plan(command, repo_root(), Arc::new(registry), Arc::new(providers_data), Arc::new(runner), config_base()).await
    }

    // -----------------------------------------------------------------------
    // Tests: build_plan
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn build_plan_create_checkout_returns_steps() {
        let mut registry = empty_registry();
        registry
            .checkout_managers
            .insert("wt".to_string(), (desc("wt"), Arc::new(MockCheckoutManager::succeeding("feat-x", "/repo/wt-feat-x"))));
        registry.workspace_manager = Some((desc("cmux"), Arc::new(MockWorkspaceManager::succeeding())));
        let data = empty_data();
        let runner = runner_ok();

        let plan = run_build_plan(
            Command::CreateCheckout { branch: "feat-x".to_string(), create_branch: true, issue_ids: vec![] },
            registry,
            data,
            runner,
        )
        .await;

        match plan {
            ExecutionPlan::Steps(step_plan) => {
                // At least 2 steps: checkout + workspace
                assert!(step_plan.steps.len() >= 2, "expected at least 2 steps, got {}", step_plan.steps.len());
            }
            ExecutionPlan::Immediate(_) => panic!("expected Steps, got Immediate"),
        }
    }

    #[tokio::test]
    async fn build_plan_create_checkout_skips_existing() {
        let mut registry = empty_registry();
        registry
            .checkout_managers
            .insert("wt".to_string(), (desc("wt"), Arc::new(MockCheckoutManager::succeeding("feat-x", "/repo/wt-feat-x"))));
        registry.workspace_manager = Some((desc("cmux"), Arc::new(MockWorkspaceManager::succeeding())));
        let mut data = empty_data();
        // Pre-populate with an existing checkout for the branch
        data.checkouts.insert(hp("/repo/wt-feat-x"), make_checkout("feat-x", "/repo/wt-feat-x"));
        let runner = runner_ok();

        let plan = run_build_plan(
            Command::CreateCheckout { branch: "feat-x".to_string(), create_branch: true, issue_ids: vec![] },
            registry,
            data,
            runner,
        )
        .await;

        match plan {
            ExecutionPlan::Steps(step_plan) => {
                // Still has steps (checkout step is present but will skip at runtime,
                // workspace step is present). The plan is structurally the same;
                // the skip happens when the step action runs.
                assert!(step_plan.steps.len() >= 2, "expected at least 2 steps, got {}", step_plan.steps.len());
            }
            ExecutionPlan::Immediate(_) => panic!("expected Steps, got Immediate"),
        }
    }

    #[tokio::test]
    async fn build_plan_teleport_session_returns_steps() {
        let mut registry = empty_registry();
        registry.cloud_agents.insert("claude".to_string(), (desc("claude"), Arc::new(MockCloudAgent::succeeding())));
        registry.workspace_manager = Some((desc("cmux"), Arc::new(MockWorkspaceManager::succeeding())));
        let mut data = empty_data();
        let path = PathBuf::from("/repo/wt-feat");
        data.checkouts.insert(hp("/repo/wt-feat"), make_checkout("feat", "/repo/wt-feat"));
        data.sessions.insert("sess-1".to_string(), make_session_for("claude", "sess-1"));
        let runner = runner_ok();

        let plan = run_build_plan(
            Command::TeleportSession { session_id: "sess-1".to_string(), branch: Some("feat".to_string()), checkout_key: Some(path) },
            registry,
            data,
            runner,
        )
        .await;

        match plan {
            ExecutionPlan::Steps(step_plan) => {
                // 3 steps: resolve attach, ensure checkout, create workspace
                assert!(step_plan.steps.len() >= 2, "expected at least 2 steps, got {}", step_plan.steps.len());
            }
            ExecutionPlan::Immediate(_) => panic!("expected Steps, got Immediate"),
        }
    }

    #[tokio::test]
    async fn build_plan_remove_checkout_returns_steps() {
        let mut registry = empty_registry();
        registry.checkout_managers.insert("wt".to_string(), (desc("wt"), Arc::new(MockCheckoutManager::succeeding("old", "/repo/wt-old"))));
        let runner = runner_ok();

        let plan =
            run_build_plan(Command::RemoveCheckout { branch: "old".to_string(), terminal_keys: vec![] }, registry, empty_data(), runner)
                .await;

        match plan {
            ExecutionPlan::Steps(step_plan) => {
                // At least 1 step: remove checkout
                assert!(!step_plan.steps.is_empty(), "expected at least 1 step");
            }
            ExecutionPlan::Immediate(_) => panic!("expected Steps, got Immediate"),
        }
    }

    #[tokio::test]
    async fn build_plan_simple_command_returns_immediate() {
        let mut registry = empty_registry();
        registry.code_review.insert("github".to_string(), (desc("github"), Arc::new(MockCodeReview)));
        let runner = runner_ok();

        let plan = run_build_plan(Command::OpenChangeRequest { id: "42".to_string() }, registry, empty_data(), runner).await;

        match plan {
            ExecutionPlan::Immediate(result) => {
                assert_ok(result);
            }
            ExecutionPlan::Steps(_) => panic!("expected Immediate, got Steps"),
        }
    }
}
